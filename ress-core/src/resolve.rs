//! Pending navigation. An operation that cannot resolve within its
//! interactive budget continues as a cancellable background scan with live
//! progress, instead of blocking the UI or silently degrading.
use crate::document::Anchor;

/// The outcome of a navigation request.
pub enum Resolution {
    /// Answered within the interactive budget.
    Ready(Anchor),
    /// Scanning continues in the background; watch `progress`, await
    /// `handle`, or `cancel`.
    Pending(PendingNav),
}
/// Live progress of a pending scan: `scanned` toward roughly `span` bytes
/// (a multi-stage jump may briefly exceed it; `percent` saturates).
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub scanned: u64,
    pub span: u64,
}
impl Progress {
    /// Whole-number percent, capped at 99 until completion.
    pub fn percent(&self) -> u64 {
        // u128 keeps the ratio exact for huge (sparse) spans; saturating u64
        // math would collapse e.g. half of u64::MAX to 1%.
        ((self.scanned as u128 * 100 / (self.span as u128).max(1)) as u64).min(99)
    }
}
/// A background navigation scan. Dropping the handle detaches the task, so
/// holders must `cancel` (or await) it; the run loop does exactly that.
pub struct PendingNav {
    /// Short human label for the progress row.
    pub label: &'static str,
    /// Progress updates, published once per scan chunk.
    pub progress: tokio::sync::watch::Receiver<Progress>,
    /// Resolves to the final anchor; abort via `cancel`.
    pub handle: tokio::task::JoinHandle<anyhow::Result<Anchor>>,
}
impl PendingNav {
    /// Aborts the background scan; the anchor never moved.
    pub fn cancel(&self) {
        self.handle.abort();
    }
}
#[cfg(test)]
impl Resolution {
    /// Test helper: unwrap an in-budget result.
    pub(crate) fn ready(self) -> Anchor {
        match self {
            Resolution::Ready(a) => a,
            Resolution::Pending(_) => panic!("expected Ready, got Pending"),
        }
    }
    /// Test helper: resolve fully, joining a pending scan.
    pub(crate) async fn join(self) -> Anchor {
        match self {
            Resolution::Ready(a) => a,
            Resolution::Pending(p) => p
                .handle
                .await
                .expect("scan task panicked")
                .expect("scan failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percent_saturates_and_guards_zero_span() {
        assert_eq!(
            Progress {
                scanned: 0,
                span: 100
            }
            .percent(),
            0
        );
        // huge sparse spans keep an exact ratio: saturating u64 math would
        // collapse this to 1%.
        assert_eq!(
            Progress {
                scanned: u64::MAX / 2,
                span: u64::MAX
            }
            .percent(),
            49
        );
        assert_eq!(
            Progress {
                scanned: 50,
                span: 100
            }
            .percent(),
            50
        );
        assert_eq!(
            Progress {
                scanned: 100,
                span: 100
            }
            .percent(),
            99
        );
        assert_eq!(
            Progress {
                scanned: 250,
                span: 100
            }
            .percent(),
            99
        );
        assert_eq!(
            Progress {
                scanned: 5,
                span: 0
            }
            .percent(),
            99
        );
        assert_eq!(
            Progress {
                scanned: 0,
                span: 0
            }
            .percent(),
            0
        );
        // the multiplication saturates rather than overflowing.
        assert_eq!(
            Progress {
                scanned: u64::MAX,
                span: 1
            }
            .percent(),
            99
        );
    }
}
