//! One-shot password exchange for explicit backup jobs. Polling reads memory
//! only. There is no password store, command-line argument, log or path input.
use crate::backup_encryption::Password;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        mpsc::{sync_channel, RecvTimeoutError, SyncSender},
        Mutex,
    },
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(300);
#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Purpose {
    Create,
    Unlock,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub id: String,
    pub purpose: Purpose,
    pub filename: String,
}
struct Pending {
    prompt: Prompt,
    expires: Instant,
    sender: SyncSender<Option<Password>>,
}
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
fn state() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}
fn expire(s: &mut Option<Pending>, now: Instant) {
    if s.as_ref().is_some_and(|p| now >= p.expires) {
        *s = None;
    }
}
pub fn status() -> Option<Prompt> {
    let mut s = state();
    expire(&mut s, Instant::now());
    s.as_ref().map(|p| p.prompt.clone())
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Answer {
    Cancel,
    Password {
        value: Password,
        confirmation: Option<Password>,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reply {
    pub id: String,
    pub answer: Answer,
}
fn validate(purpose: Purpose, answer: Answer) -> Result<Option<Password>, String> {
    match answer {
        Answer::Cancel => Ok(None),
        Answer::Password {
            value,
            confirmation,
        } => {
            match purpose {
                Purpose::Create => {
                    value.validate_creation().map_err(|e| e.to_string())?;
                    if confirmation.as_ref().is_none_or(|c| !value.matches(c)) {
                        return Err("两次输入的备份密码不一致。".into());
                    }
                }
                Purpose::Unlock => {
                    if confirmation.is_some() {
                        return Err("打开备份不接受额外确认字段。".into());
                    }
                }
            }
            Ok(Some(value))
        }
    }
}
fn deliver(s: &mut Option<Pending>, reply: Reply, now: Instant) -> Result<(), String> {
    expire(s, now);
    let p = s.as_ref().ok_or("密码请求已结束或过期。")?;
    if p.prompt.id != reply.id {
        return Err("密码请求已变化，请重新打开。".into());
    }
    let answer = validate(p.prompt.purpose, reply.answer)?;
    let p = s.take().ok_or("密码请求已结束。")?;
    p.sender
        .try_send(answer)
        .map_err(|_| "密码请求已结束，没有保存输入内容。".into())
}
pub fn submit(reply: Reply) -> Result<(), String> {
    deliver(&mut state(), reply, Instant::now())
}
struct Guard(String);
impl Drop for Guard {
    fn drop(&mut self) {
        let mut s = state();
        if s.as_ref().is_some_and(|p| p.prompt.id == self.0) {
            *s = None;
        }
    }
}
/// Must run on the caller's background job thread, never the UI thread. `tick`
/// checks cancellation. Returning None cancels the operation, never disables
/// encryption or silently tries another password.
pub fn ask(
    purpose: Purpose,
    filename: &str,
    mut tick: impl FnMut() -> Result<(), String>,
) -> Result<Option<Password>, String> {
    tick()?;
    let id = uuid::Uuid::new_v4().to_string();
    let prompt = Prompt {
        id: id.clone(),
        purpose,
        filename: filename
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect(),
    };
    let (sender, receiver) = sync_channel(1);
    let expires = Instant::now() + TTL;
    {
        let mut s = state();
        expire(&mut s, Instant::now());
        if s.is_some() {
            return Err("另一个备份密码请求尚未结束。".into());
        }
        *s = Some(Pending {
            prompt,
            expires,
            sender,
        });
    }
    let _guard = Guard(id);
    loop {
        tick()?;
        if Instant::now() >= expires {
            return Err("备份密码输入已过期，未继续操作。".into());
        }
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(answer) => {
                tick()?;
                return Ok(answer);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("备份密码请求已结束或过期。".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn password(s: &str) -> Password {
        Password::new(s.into()).unwrap()
    }
    fn pending(purpose: Purpose) -> (Option<Pending>, std::sync::mpsc::Receiver<Option<Password>>) {
        let (sender, receiver) = sync_channel(1);
        (
            Some(Pending {
                prompt: Prompt {
                    id: "request".into(),
                    purpose,
                    filename: "synthetic.dsbackup".into(),
                },
                expires: Instant::now() + TTL,
                sender,
            }),
            receiver,
        )
    }
    #[test]
    fn secrets_are_one_shot_bound_and_confirmation_is_checked() {
        let (mut p, r) = pending(Purpose::Create);
        assert!(deliver(
            &mut p,
            Reply {
                id: "stale".into(),
                answer: Answer::Cancel
            },
            Instant::now()
        )
        .is_err());
        assert!(p.is_some());
        assert!(deliver(
            &mut p,
            Reply {
                id: "request".into(),
                answer: Answer::Password {
                    value: password("test-password"),
                    confirmation: Some(password("different"))
                }
            },
            Instant::now()
        )
        .is_err());
        assert!(p.is_some());
        deliver(
            &mut p,
            Reply {
                id: "request".into(),
                answer: Answer::Password {
                    value: password("test-password"),
                    confirmation: Some(password("test-password")),
                },
            },
            Instant::now(),
        )
        .unwrap();
        assert!(p.is_none());
        assert!(r
            .try_recv()
            .unwrap()
            .unwrap()
            .matches(&password("test-password")));
        assert!(deliver(
            &mut p,
            Reply {
                id: "request".into(),
                answer: Answer::Cancel
            },
            Instant::now()
        )
        .is_err());
    }
    #[test]
    fn secrets_expire_cancel_and_reject_unexpected_ipc_fields() {
        let (mut p, r) = pending(Purpose::Unlock);
        let expiry = p.as_ref().unwrap().expires;
        assert!(deliver(
            &mut p,
            Reply {
                id: "request".into(),
                answer: Answer::Cancel
            },
            expiry
        )
        .is_err());
        assert!(r.try_recv().is_err());
        assert!(p.is_none());
        let (mut p, r) = pending(Purpose::Unlock);
        deliver(
            &mut p,
            Reply {
                id: "request".into(),
                answer: Answer::Cancel,
            },
            Instant::now(),
        )
        .unwrap();
        assert!(r.try_recv().unwrap().is_none());
        assert!(serde_json::from_str::<Reply>(
            r#"{"id":"a","answer":{"kind":"password","value":"secret","path":"C:/disk"}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<Reply>(
            r#"{"id":"a","answer":{"kind":"password","value":""}}"#
        )
        .is_err());
        assert!(validate(
            Purpose::Unlock,
            Answer::Password {
                value: password("test-password"),
                confirmation: Some(password("test-password"))
            }
        )
        .is_err());
        let prompt = Prompt {
            id: "x".into(),
            purpose: Purpose::Unlock,
            filename: "synthetic.dsbackup".into(),
        };
        let value = serde_json::to_value(prompt).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 3);
    }
}
