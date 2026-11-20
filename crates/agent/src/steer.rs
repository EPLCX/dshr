//! 插队：一轮运行期间用户发来的消息不必等这一轮结束，交给 [`SteerQueue`]，
//! agent 在当前这一步结束后把它接进对话（见 [`crate::Agent::with_steering`]）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::UserAttachment;

/// 一条插队的消息。`id` 由界面给，用来认出哪条已经被接进对话、哪条还在等。
#[derive(Debug, Clone, PartialEq)]
pub struct SteerMessage {
    pub id: u64,
    pub text: String,
    pub attachments: Vec<UserAttachment>,
}

/// 界面和 agent 共用的插队队列。克隆的是同一个队列。
#[derive(Debug, Clone, Default)]
pub struct SteerQueue(Arc<Mutex<VecDeque<SteerMessage>>>);

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<SteerMessage>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn push(&self, message: SteerMessage) {
        self.lock().push_back(message);
    }

    /// 还在等的那条消息（按 `id`）。
    pub fn contains(&self, id: u64) -> bool {
        self.lock().iter().any(|message| message.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// 取回一条还在等的消息；已经被取走时返回假。
    pub fn remove(&self, id: u64) -> bool {
        let mut queue = self.lock();
        let before = queue.len();
        queue.retain(|message| message.id != id);
        queue.len() != before
    }

    /// 是不是同一个队列（克隆出来的算同一个）。
    pub fn same(&self, other: &SteerQueue) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// 取出所有还在等的消息（agent 接进对话时；或者这一轮结束了，界面把没赶上的收回去）。
    pub fn take_all(&self) -> Vec<SteerMessage> {
        self.lock().drain(..).collect()
    }
}
