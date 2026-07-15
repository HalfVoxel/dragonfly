//! Status lines that defer around interactive prompts.
//!
//! Background tasks (dedup hints, relevant-context scoring, PR-area
//! analysis) run while `dragonfly` may be blocked on a dialoguer prompt,
//! and a line printed mid-prompt clobbers the user's input. Progress
//! prints from those tasks go through [line] so the prompt can hold them
//! back and flush once the read completes.

use std::sync::Mutex;

static PENDING: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Print a background status line to stderr, or buffer it while an
/// interactive prompt holds the terminal (see [hold]).
pub fn line(msg: String) {
    let mut pending = PENDING.lock().unwrap();
    match pending.as_mut() {
        Some(buf) => buf.push(msg),
        None => eprintln!("{msg}"),
    }
}

/// Defer [line] output until the returned guard drops, then flush in
/// arrival order. Holds are not nestable: the first guard to drop
/// flushes. Only [line] output is deferred; direct println!/eprintln!
/// still reaches the terminal.
pub fn hold() -> Hold {
    let mut pending = PENDING.lock().unwrap();
    if pending.is_none() {
        *pending = Some(Vec::new());
    }
    Hold
}

pub struct Hold;

impl Drop for Hold {
    fn drop(&mut self) {
        let buffered = PENDING.lock().unwrap().take();
        for msg in buffered.into_iter().flatten() {
            eprintln!("{msg}");
        }
    }
}

macro_rules! status_line {
    ($($arg:tt)*) => {
        $crate::status::line(format!($($arg)*))
    };
}
pub(crate) use status_line;

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: a status line emitted while a Hold is open must not
    // reach the terminal until the Hold drops (it used to interleave
    // with dialoguer's title prompt and clobber typed input).
    #[test]
    fn hold_buffers_then_flushes() {
        let hold = hold();
        line("one".into());
        line("two".into());
        assert_eq!(
            PENDING.lock().unwrap().as_deref(),
            Some(&["one".to_string(), "two".to_string()][..])
        );
        drop(hold);
        assert_eq!(*PENDING.lock().unwrap(), None);
    }
}
