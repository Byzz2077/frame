use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, TryRecvError},
};

use frame_core::size_estimate::SizeEstimate;

use super::*;
use crate::{
    conversion_runner::core_config_from_gpui,
    size_estimate::{SizeEstimateProgress, run_size_estimate},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::app) enum SizeEstimateStatus {
    Running(SizeEstimateProgress),
    Ready(SizeEstimate),
    Failed(String),
}

/// The latest estimate, which only applies to the settings it was started with.
pub(in crate::app) struct SizeEstimateRun {
    file_id: String,
    settings_key: String,
    pub(in crate::app) status: SizeEstimateStatus,
    cancel: Arc<AtomicBool>,
}

enum SizeEstimateMessage {
    Progress(SizeEstimateProgress),
    Finished(Option<SizeEstimate>),
    Failed(String),
}

fn settings_key(config: &ConversionConfig) -> String {
    serde_json::to_string(&core_config_from_gpui(config)).unwrap_or_default()
}

impl FrameRoot {
    pub(in crate::app) fn cancel_size_estimate(&mut self) {
        if let Some(run) = self.size_estimate.take() {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(in crate::app) fn start_size_estimate(&mut self, cx: &Context<Self>) {
        self.cancel_size_estimate();
        let Some(file) = self.file_queue.selected_file() else {
            return;
        };

        let file_id = file.id.clone();
        let file_path = file.path.clone();
        let config = core_config_from_gpui(&file.config);
        let cancel = Arc::new(AtomicBool::new(false));
        self.size_estimate = Some(SizeEstimateRun {
            file_id,
            settings_key: settings_key(&file.config),
            status: SizeEstimateStatus::Running(SizeEstimateProgress::Preparing),
            cancel: cancel.clone(),
        });

        let (tx, rx) = mpsc::channel();
        let worker_cancel = cancel.clone();
        cx.background_spawn(async move {
            let result = run_size_estimate(&file_path, &config, &worker_cancel, &mut |progress| {
                let _ = tx.send(SizeEstimateMessage::Progress(progress));
            });
            let _ = tx.send(match result {
                Ok(estimate) => SizeEstimateMessage::Finished(estimate),
                Err(error) => SizeEstimateMessage::Failed(error.to_string()),
            });
        })
        .detach();

        cx.spawn(async move |this, cx| {
            loop {
                let is_finished = loop {
                    match rx.try_recv() {
                        Ok(message) => {
                            let is_last = !matches!(message, SizeEstimateMessage::Progress(_));
                            if this
                                .update(cx, |root, cx| {
                                    root.apply_size_estimate_message(&cancel, message);
                                    cx.notify();
                                })
                                .is_err()
                            {
                                cancel.store(true, Ordering::Relaxed);
                                return;
                            }
                            if is_last {
                                break true;
                            }
                        }
                        Err(TryRecvError::Empty) => break false,
                        Err(TryRecvError::Disconnected) => break true,
                    }
                };
                if is_finished {
                    return;
                }

                let is_stale = this
                    .update(cx, |root, cx| {
                        let is_stale = root.cancel_size_estimate_if_stale(&cancel);
                        if is_stale {
                            cx.notify();
                        }
                        is_stale
                    })
                    .unwrap_or(true);
                if is_stale {
                    cancel.store(true, Ordering::Relaxed);
                    return;
                }

                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    /// Returns the estimate when it still matches the selected file and its settings.
    pub(in crate::app) fn current_size_estimate(&self) -> Option<&SizeEstimateStatus> {
        let run = self.size_estimate.as_ref()?;
        let file = self.file_queue.selected_file()?;
        (run.file_id == file.id && run.settings_key == settings_key(&file.config))
            .then_some(&run.status)
    }

    fn apply_size_estimate_message(
        &mut self,
        cancel: &Arc<AtomicBool>,
        message: SizeEstimateMessage,
    ) {
        let Some(run) = self
            .size_estimate
            .as_mut()
            .filter(|run| Arc::ptr_eq(&run.cancel, cancel))
        else {
            return;
        };

        match message {
            SizeEstimateMessage::Progress(progress) => {
                run.status = SizeEstimateStatus::Running(progress);
            }
            SizeEstimateMessage::Finished(Some(estimate)) => {
                run.status = SizeEstimateStatus::Ready(estimate);
            }
            SizeEstimateMessage::Finished(None) => self.size_estimate = None,
            SizeEstimateMessage::Failed(error) => run.status = SizeEstimateStatus::Failed(error),
        }
    }

    /// Stops a run once the user changes the settings it is measuring.
    fn cancel_size_estimate_if_stale(&mut self, cancel: &Arc<AtomicBool>) -> bool {
        let Some(run) = self
            .size_estimate
            .as_ref()
            .filter(|run| Arc::ptr_eq(&run.cancel, cancel))
        else {
            return true;
        };

        let is_current = self
            .file_queue
            .file_by_id(&run.file_id)
            .is_some_and(|file| settings_key(&file.config) == run.settings_key);
        if !is_current {
            self.cancel_size_estimate();
        }
        !is_current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with_finished_estimate() -> FrameRoot {
        let mut root = FrameRoot::new();
        root.file_queue
            .add_file(FileItem::from_path("first", "/tmp/one.mp4", 1));
        root.file_queue
            .add_file(FileItem::from_path("second", "/tmp/two.mp4", 1));
        root.file_queue.select_existing_file("first");
        let config = root.selected_config().expect("selected file").clone();
        root.size_estimate = Some(SizeEstimateRun {
            file_id: "first".to_string(),
            settings_key: settings_key(&config),
            status: SizeEstimateStatus::Failed("sample".to_string()),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        root
    }

    #[test]
    fn estimate_is_hidden_after_its_settings_change() {
        let mut root = root_with_finished_estimate();
        assert!(root.current_size_estimate().is_some());

        root.update_selected_config(|config| {
            config.crf += 1;
            true
        });

        assert!(root.current_size_estimate().is_none());
    }

    #[test]
    fn estimate_only_applies_to_its_own_file() {
        let mut root = root_with_finished_estimate();

        root.file_queue.select_existing_file("second");

        assert!(root.current_size_estimate().is_none());
    }

    #[test]
    fn stale_run_is_cancelled_while_it_is_running() {
        let mut root = root_with_finished_estimate();
        let cancel = root.size_estimate.as_ref().expect("run").cancel.clone();
        assert!(!root.cancel_size_estimate_if_stale(&cancel));

        root.update_selected_config(|config| {
            config.crf += 1;
            true
        });

        assert!(root.cancel_size_estimate_if_stale(&cancel));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(root.size_estimate.is_none());
    }
}
