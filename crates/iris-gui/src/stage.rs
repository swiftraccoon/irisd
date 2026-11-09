//! The stage: one GStreamer pipeline shared by every live tile. Each tile is
//! a bin added to and removed from it, so all tiles share one GL context and
//! one GL thread, where a pipeline per tile would run one of each.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Context;
use gst::prelude::*;
use gtk::glib;
use tracing::warn;

/// What a tile learns about its branch: errors from the stage's bus, readiness
/// from the branch's own sink pad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchEvent {
    /// The branch posted an error; the text is the error's message.
    Error(String),
    /// The branch's sink received its first frame since the branch started
    /// or last restarted.
    Ready,
}

/// The shared pipeline and the listeners of its branches by bin name.
#[derive(Debug)]
pub struct Stage {
    pipeline: gst::Pipeline,
    listeners: Rc<RefCell<HashMap<String, async_channel::Sender<BranchEvent>>>>,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Stage {
    /// Builds the pipeline and sets it playing; it stays playing while the
    /// viewer runs, empty or not.
    pub fn new() -> anyhow::Result<Rc<Self>> {
        let pipeline = gst::Pipeline::new();
        let listeners: Rc<RefCell<HashMap<String, async_channel::Sender<BranchEvent>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let bus = pipeline.bus().context("stage bus")?;
        let bus_watch = bus
            .add_watch_local({
                let listeners = Rc::clone(&listeners);
                move |_, message| {
                    let branch = message.src().and_then(branch_name);
                    match message.view() {
                        gst::MessageView::Error(error) => {
                            warn!(branch = ?branch, error = %error.error(), "branch error");
                            if let Some(name) = branch {
                                notify(
                                    &listeners,
                                    &name,
                                    BranchEvent::Error(error.error().to_string()),
                                );
                            }
                        }
                        gst::MessageView::Warning(warning) => {
                            warn!(branch = ?branch, warning = %warning.error(), "branch warning");
                        }
                        _ => {}
                    }
                    glib::ControlFlow::Continue
                }
            })
            .context("watching the stage bus")?;
        pipeline
            .set_state(gst::State::Playing)
            .context("starting the stage")?;
        Ok(Rc::new(Self {
            pipeline,
            listeners,
            _bus_watch: bus_watch,
        }))
    }

    /// Adds `bin` under `name` and brings it to the pipeline's state; `events`
    /// receives the branch's errors and readiness.
    pub fn add(
        &self,
        name: &str,
        bin: &gst::Bin,
        events: async_channel::Sender<BranchEvent>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            bin.parent().is_none(),
            "adding {name}: bin already has a parent"
        );
        anyhow::ensure!(
            !self.listeners.borrow().contains_key(name),
            "adding {name}: branch name is already in use"
        );
        let original_name = bin.name().to_string();
        let (_, original_state, _) = bin.state(gst::ClockTime::ZERO);
        bin.set_property("name", name);
        if let Err(error) = self.pipeline.add(bin) {
            bin.set_property("name", &original_name);
            return Err(error).with_context(|| format!("adding {name} to the stage"));
        }
        self.listeners.borrow_mut().insert(name.to_owned(), events);
        if let Err(error) = bin.sync_state_with_parent() {
            let rollback = self.rollback_add(name, bin, &original_name, original_state);
            return match rollback {
                Ok(()) => Err(error).with_context(|| format!("starting {name}")),
                Err(rollback) => Err(anyhow::anyhow!(
                    "starting {name}: {error}; rolling back the failed branch: {rollback:#}"
                )),
            };
        }
        Ok(())
    }

    fn rollback_add(
        &self,
        name: &str,
        bin: &gst::Bin,
        original_name: &str,
        original_state: gst::State,
    ) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        if let Err(error) = bin.set_state(gst::State::Null) {
            failures.push(format!("stopping partially added {name}: {error}"));
        }
        if let Err(error) = self.pipeline.remove(bin) {
            failures.push(format!("removing partially added {name}: {error}"));
        }
        self.listeners.borrow_mut().remove(name);
        if bin.parent().is_none() {
            bin.set_property("name", original_name);
            if original_state != gst::State::Null
                && let Err(error) = bin.set_state(original_state)
            {
                failures.push(format!("restoring {original_name} state: {error}"));
            }
        } else {
            failures.push(format!("partially added {name} remains parented"));
        }
        if !failures.is_empty() {
            anyhow::bail!(failures.join("; "));
        }
        Ok(())
    }

    /// Restarts `bin` in place after its feed's session ends: down to Null and
    /// back to the pipeline's state, so the next session's init segment meets a
    /// clean demuxer.
    pub fn restart(bin: &gst::Bin) -> anyhow::Result<()> {
        bin.set_state(gst::State::Null)
            .context("stopping the branch")?;
        bin.sync_state_with_parent()
            .context("restarting the branch")?;
        Ok(())
    }

    /// Stops and removes `bin`.
    pub fn remove(&self, bin: &gst::Bin) {
        let name = bin.name().to_string();
        if let Err(error) = bin.set_state(gst::State::Null) {
            warn!(%name, %error, "cannot stop a branch");
        }
        if let Err(error) = self.pipeline.remove(bin) {
            warn!(%name, %error, "cannot remove a branch");
        }
        self.listeners.borrow_mut().remove(&name);
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Err(error) = self.pipeline.set_state(gst::State::Null) {
            warn!(%error, "cannot stop the stage");
        }
    }
}

/// The name of the branch bin an element belongs to: the child of the pipeline
/// on the element's parent chain.
fn branch_name(object: &gst::Object) -> Option<String> {
    let mut current = object.clone();
    loop {
        let parent = current.parent()?;
        if parent.parent().is_none() {
            // `parent` is the pipeline; `current` is the branch.
            return current
                .downcast_ref::<gst::Bin>()
                .map(|bin| bin.name().to_string());
        }
        current = parent;
    }
}

fn notify(
    listeners: &RefCell<HashMap<String, async_channel::Sender<BranchEvent>>>,
    name: &str,
    event: BranchEvent,
) {
    if let Some(sender) = listeners.borrow().get(name) {
        let _ = sender.try_send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_is_transactional_across_failure_modes() {
        gst::init().unwrap();
        let stage = Stage::new().unwrap();

        {
            let owner = gst::Pipeline::new();
            let bin = gst::Bin::new();
            let original_name = bin.name().to_string();
            owner.add(&bin).unwrap();
            let (events, _) = async_channel::unbounded();

            assert!(stage.add("candidate", &bin, events).is_err());
            assert_eq!(bin.name(), original_name);
            assert_eq!(bin.parent().as_ref(), Some(owner.upcast_ref()));
            assert!(!stage.listeners.borrow().contains_key("candidate"));
        }

        {
            let first = gst::Bin::new();
            let second = gst::Bin::new();
            let second_name = second.name().to_string();
            let (first_tx, first_rx) = async_channel::unbounded();
            stage.add("duplicate", &first, first_tx).unwrap();
            let (second_tx, second_rx) = async_channel::unbounded();

            assert!(stage.add("duplicate", &second, second_tx).is_err());
            notify(&stage.listeners, "duplicate", BranchEvent::Ready);
            assert_eq!(first_rx.try_recv().unwrap(), BranchEvent::Ready);
            assert!(second_rx.try_recv().is_err());
            assert_eq!(second.name(), second_name);
            assert!(second.parent().is_none());
            stage.remove(&first);
        }

        {
            let bin = gst::parse::bin_from_description(
                "fakesink async=false state-error=paused-to-playing",
                /* ghost_unlinked_pads */ false,
            )
            .unwrap();
            bin.set_state(gst::State::Ready).unwrap();
            let original_name = bin.name().to_string();
            let (events, _) = async_channel::unbounded();

            assert!(stage.add("failing", &bin, events).is_err());
            let (_, restored_state, _) = bin.state(gst::ClockTime::ZERO);
            assert_eq!(bin.name(), original_name);
            assert_eq!(restored_state, gst::State::Ready);
            assert!(bin.parent().is_none());
            assert!(!stage.listeners.borrow().contains_key("failing"));
            bin.set_state(gst::State::Null).unwrap();
        }
    }
}
