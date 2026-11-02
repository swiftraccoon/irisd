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
    /// The branch's sink received its first frame.
    Ready,
}

/// The shared pipeline and the listeners of its branches by bin name.
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
        bin.set_property("name", name);
        self.listeners.borrow_mut().insert(name.to_owned(), events);
        self.pipeline
            .add(bin)
            .with_context(|| format!("adding {name} to the stage"))?;
        bin.sync_state_with_parent()
            .with_context(|| format!("starting {name}"))?;
        Ok(())
    }

    /// Restarts `bin` in place (after a reconnection): down to Null and back
    /// to the pipeline's state, so its demuxer starts clean.
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
        let _ = bin.set_state(gst::State::Null);
        if let Err(error) = self.pipeline.remove(bin) {
            warn!(%name, %error, "cannot remove a branch");
        }
        self.listeners.borrow_mut().remove(&name);
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
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
