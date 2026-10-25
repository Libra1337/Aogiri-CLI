//! Which queued message a client's queue op means (`steer`, `unqueue`,
//! `queue_move`, `interrupt` with `index`), the same for every engine.
//!
//! A client names a message by its place in the `pending_messages` it saw:
//! `index` counts from the first (0), or from the last when negative (`-1`
//! is the message just sent, so a client can send and steer it at once
//! without knowing the queue's length). With `text` too, the message at
//! `index` counts only when its text is `text`, else the first with that
//! text: the queue may have moved on since the client saw it.

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuePick {
    /// Absent: the first.
    pub index: Option<i64>,
    pub text: Option<String>,
}

impl QueuePick {
    /// `index` and `text` of a client op.
    pub fn from_op(op: &Value) -> Self {
        Self {
            index: op.get("index").and_then(Value::as_i64),
            text: op.get("text").and_then(Value::as_str).map(str::to_string),
        }
    }

    /// The place of the message meant among `queued` (their texts, as
    /// `pending_messages` lists them), if it is there.
    pub fn find<S: AsRef<str>>(&self, queued: &[S]) -> Option<usize> {
        let at = queue_index(self.index, queued.len());
        match &self.text {
            None => at,
            Some(text) => at
                .filter(|at| queued[*at].as_ref() == text)
                .or_else(|| queued.iter().position(|queued| queued.as_ref() == text)),
        }
    }
}

/// A queued message's place from a client's `index` (the first when absent;
/// negative counts from the last), if there is one there.
pub fn queue_index(index: Option<i64>, len: usize) -> Option<usize> {
    let index = index.unwrap_or(0);
    let at = if index < 0 {
        len.checked_sub(index.unsigned_abs() as usize)?
    } else {
        index as usize
    };
    (at < len).then_some(at)
}

/// Where a queued message moves (`queue_move`): message `pick` to place `to`
/// (negative from the last); None when either is not there or it would stay
/// put.
pub fn queue_move_target<S: AsRef<str>>(
    pick: &QueuePick,
    to: Option<i64>,
    queued: &[S],
) -> Option<(usize, usize)> {
    let from = pick.find(queued)?;
    let to = queue_index(Some(to?), queued.len())?;
    (from != to).then_some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pick(op: Value) -> QueuePick {
        QueuePick::from_op(&op)
    }

    #[test]
    fn an_index_counts_from_the_first_or_from_the_last() {
        let queued = ["one", "two", "three"];
        assert_eq!(pick(json!({})).find(&queued), Some(0));
        assert_eq!(pick(json!({ "index": 1 })).find(&queued), Some(1));
        assert_eq!(pick(json!({ "index": -1 })).find(&queued), Some(2));
        assert_eq!(pick(json!({ "index": -3 })).find(&queued), Some(0));
        assert_eq!(pick(json!({ "index": 3 })).find(&queued), None);
        assert_eq!(pick(json!({ "index": -4 })).find(&queued), None);
        assert_eq!(pick(json!({})).find::<&str>(&[]), None);
    }

    #[test]
    fn a_text_finds_the_message_after_the_queue_moved_on() {
        let queued = ["two", "three"];
        // The client saw ["one", "two", "three"] and picked "two" at 1.
        assert_eq!(
            pick(json!({ "index": 1, "text": "two" })).find(&queued),
            Some(0)
        );
        assert_eq!(
            pick(json!({ "index": 1, "text": "three" })).find(&queued),
            Some(1)
        );
        // Started meanwhile: nothing to take.
        assert_eq!(pick(json!({ "index": 0, "text": "one" })).find(&queued), None);
    }

    #[test]
    fn a_move_needs_two_places_that_differ() {
        let queued = ["a", "b", "c"];
        let from_last = pick(json!({ "index": -1 }));
        assert_eq!(queue_move_target(&from_last, Some(0), &queued), Some((2, 0)));
        assert_eq!(queue_move_target(&from_last, Some(-1), &queued), None);
        assert_eq!(queue_move_target(&from_last, None, &queued), None);
        assert_eq!(queue_move_target(&from_last, Some(5), &queued), None);
    }
}
