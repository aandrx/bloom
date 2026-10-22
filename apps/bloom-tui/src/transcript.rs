//! Reading positions expressed in message-owned display rows, not distance from
//! a changing tail. Geometry comes only from the transcript's actual formatter.
use bloom_domain::{ConversationId, MessageId};

#[derive(Clone, Debug)]
pub struct MessageRows {
    pub id: MessageId,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct ReadingAnchor {
    pub conversation: ConversationId,
    pub message: MessageId,
    pub row: usize,
}

#[derive(Clone, Debug)]
pub struct TranscriptViewport {
    pub conversation: ConversationId,
    pub messages: Vec<MessageRows>,
    pub rows: usize,
    pub height: usize,
    /// Actual first row after clamping and the no-orphan-byline adjustment.
    pub top: usize,
}

impl TranscriptViewport {
    pub fn max_scroll(&self) -> usize {
        self.rows.saturating_sub(self.height)
    }

    pub fn back(&self) -> usize {
        self.max_scroll().saturating_sub(self.top)
    }

    pub fn anchor_at(&self, top: usize) -> Option<ReadingAnchor> {
        let message = self
            .messages
            .iter()
            .rev()
            .find(|message| message.start <= top)?;
        Some(ReadingAnchor {
            conversation: self.conversation.clone(),
            message: message.id.clone(),
            row: top.saturating_sub(message.start),
        })
    }

    pub fn top_for(&self, back: usize, anchor: Option<&ReadingAnchor>) -> usize {
        if back == 0 {
            return self.max_scroll();
        }
        anchor
            .filter(|anchor| anchor.conversation == self.conversation)
            .and_then(|anchor| {
                self.messages
                    .iter()
                    .find(|message| message.id == anchor.message)
                    .map(|message| {
                        message.start
                            + anchor
                                .row
                                .min(message.end.saturating_sub(message.start + 1))
                    })
            })
            // If history no longer contains the anchor, clamp the old distance.
            .unwrap_or_else(|| self.max_scroll().saturating_sub(back))
            .min(self.max_scroll())
    }
}
