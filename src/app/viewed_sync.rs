//! Keep `r` and GitHub's per-file "Viewed" checkbox in step: a toggle here
//! ticks the box there, and files ticked there open as reviewed here.
//!
//! Both directions run off the main thread, so marking files never waits on
//! `gh`.

use super::*;
use crate::forge::traits::{ForgeKind, PrSessionKey, PullRequestDetails};

impl App {
    /// Queue a viewed-state push for `path`, starting the worker on first use.
    /// No-op unless syncing a GitHub PR.
    pub(in crate::app) fn push_viewed_state(&mut self, path: &Path, viewed: bool) {
        let Some(key) = self.viewed_sync_key().cloned() else {
            return;
        };

        // A worker serves one PR session; another PR or head replaces it.
        if self
            .viewed_sync
            .as_ref()
            .is_none_or(|worker| worker.key != key)
        {
            let Some((key, details)) = self.viewed_sync_target() else {
                return;
            };
            self.spawn_viewed_sync(key, details);
        }

        let Some(worker) = self.viewed_sync.as_ref() else {
            return;
        };
        let request = ViewedSyncRequest::Set {
            path: path.to_path_buf(),
            viewed,
        };
        if worker.tx.send(request).is_err() {
            // The worker is gone; its last failure was already reported.
            self.viewed_sync = None;
        }
    }

    /// The PR session to sync with, if any. Asked every tick, so it borrows.
    fn viewed_sync_key(&self) -> Option<&PrSessionKey> {
        if !self.forge_config.sync_viewed {
            return None;
        }
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return None;
        };
        // Only GitHub has a per-viewer file state.
        (pr.key.repository.kind == ForgeKind::GitHub).then_some(&pr.key)
    }

    /// That session with the PR details a worker needs.
    fn viewed_sync_target(&self) -> Option<(PrSessionKey, PullRequestDetails)> {
        let key = self.viewed_sync_key()?.clone();
        let details = self.pr_info.as_ref()?.details.clone();
        // Never sync a PR other than the one on screen.
        (details.repository == key.repository && details.number == key.number)
            .then_some((key, details))
    }

    fn spawn_viewed_sync(&mut self, key: PrSessionKey, details: PullRequestDetails) {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<ViewedSyncRequest>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<ViewedSyncEvent>();

        let local_checkout = self
            .forge_backend
            .as_deref()
            .and_then(|backend| backend.local_checkout_path());
        let show_pr_checks = self.show_pr_checks;
        let show_pr_comments = self.show_pr_comments;

        std::thread::spawn(move || {
            // Its own backend: `App::forge_backend` cannot cross threads, and
            // this one caches the PR node id the mutation needs.
            let backend = create_forge_backend(
                &details.repository,
                local_checkout,
                show_pr_checks,
                show_pr_comments,
            );
            // Ends when the sender is dropped.
            while let Ok(ViewedSyncRequest::Set { path, viewed }) = request_rx.recv() {
                if let Err(error) = backend.set_file_viewed(&details, &path, viewed) {
                    let failure = ViewedSyncEvent::Failed {
                        path,
                        viewed,
                        error: error.to_string(),
                    };
                    if event_tx.send(failure).is_err() {
                        return;
                    }
                }
            }
        });

        self.viewed_sync = Some(ViewedSyncWorker {
            key,
            tx: request_tx,
        });
        self.viewed_sync_rx = Some(event_rx);
    }

    /// Start this session's one read of the forge's state and drain what came
    /// back. Returns whether a repaint is needed.
    pub fn poll_viewed_sync_events(&mut self) -> bool {
        let mut needs_redraw = self.start_viewed_seed();
        needs_redraw |= self.drain_viewed_seed();
        needs_redraw |= self.drain_viewed_failures();
        needs_redraw
    }

    /// Read the forge's viewed state once per PR session. Started from the
    /// main loop: `forge_config` is applied after `App::new`, too late for
    /// the open paths.
    fn start_viewed_seed(&mut self) -> bool {
        // Cheap check first: asked every tick.
        match self.viewed_sync_key() {
            Some(key) if self.viewed_seeded.as_ref() != Some(key) => {}
            _ => return false,
        }
        let Some((key, details)) = self.viewed_sync_target() else {
            return false;
        };
        // Claim the session before spawning: the read happens once even if it
        // fails, so a PR whose files GitHub will not report does not queue a
        // fresh request on every tick.
        self.viewed_seeded = Some(key.clone());

        let (tx, rx) = std::sync::mpsc::channel();
        self.viewed_seed_rx = Some(rx);

        let local_checkout = self
            .forge_backend
            .as_deref()
            .and_then(|backend| backend.local_checkout_path());
        let show_pr_checks = self.show_pr_checks;
        let show_pr_comments = self.show_pr_comments;

        std::thread::spawn(move || {
            let backend = create_forge_backend(
                &details.repository,
                local_checkout,
                show_pr_checks,
                show_pr_comments,
            );
            let result = backend
                .list_viewed_files(&details)
                .map_err(|error| error.to_string());
            let _ = tx.send(ViewedSeedEvent::Done { key, result });
        });

        false
    }

    /// Apply a finished read, if one has landed.
    fn drain_viewed_seed(&mut self) -> bool {
        let Some(rx) = self.viewed_seed_rx.as_ref() else {
            return false;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.viewed_seed_rx = None;
                return false;
            }
        };
        self.viewed_seed_rx = None;

        let ViewedSeedEvent::Done { key, result } = event;
        // A read for a PR no longer open.
        if self.viewed_sync_key() != Some(&key) {
            return false;
        }

        match result {
            Ok(paths) => self.apply_remote_viewed_state(&paths),
            Err(error) => {
                self.set_warning(format!(
                    "GitHub: could not read viewed state \u{00b7} {error}"
                ));
                true
            }
        }
    }

    /// Mark reviewed the files the forge reports as viewed. Only ever adds: a
    /// box unticked on the web, often by accident, never unmarks a file.
    pub(in crate::app) fn apply_remote_viewed_state(&mut self, paths: &[PathBuf]) -> bool {
        let mut marked = 0;
        for path in paths {
            // Only files this review covers.
            if let Some(review) = self.session.get_file_mut(path)
                && !review.reviewed
            {
                review.reviewed = true;
                marked += 1;
            }
        }
        if marked == 0 {
            return false;
        }

        self.dirty = true;
        // Collapsed files can leave the cursor past the end.
        self.rebuild_annotations();
        self.diff_state.cursor_line = self.diff_state.cursor_line.min(self.max_cursor_line());
        self.ensure_cursor_visible();
        self.set_message(format!(
            "{marked} file(s) already viewed on GitHub \u{00b7} marked reviewed"
        ));
        true
    }

    /// Drain push failures into the status bar.
    fn drain_viewed_failures(&mut self) -> bool {
        let Some(rx) = self.viewed_sync_rx.as_ref() else {
            return false;
        };

        // Show only the newest failure: a bad token fails every push.
        let mut warning = None;
        loop {
            match rx.try_recv() {
                Ok(ViewedSyncEvent::Failed {
                    path,
                    viewed,
                    error,
                }) => {
                    let verb = if viewed { "mark" } else { "unmark" };
                    // The local marker stays as the user left it: losing a
                    // toggle under the cursor is worse than a stale checkbox
                    // on GitHub, which `:reload` will resync anyway.
                    warning = Some(format!(
                        "GitHub: could not {verb} {} as viewed \u{00b7} {error}",
                        path.display()
                    ));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.viewed_sync_rx = None;
                    self.viewed_sync = None;
                    break;
                }
            }
        }

        match warning {
            Some(text) => {
                self.set_warning(text);
                true
            }
            None => false,
        }
    }
}
