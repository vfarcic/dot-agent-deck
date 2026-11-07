//! Frozen v0.49.2 request and timeout policy. See provenance.json.
//! The adapter supplies framing/Hello and omits desktop form resolution;
//! request construction, timeout, lease classification and warning text are frozen.
#![allow(dead_code)]
use dot_agent_deck::daemon_client::{AnswerReport, ClientError, DaemonCapabilities};
use dot_agent_deck::question::AnswerRefusal;
use dot_agent_deck::daemon_protocol::{AttachResponse, KIND_REQ, KIND_RESP, read_frame, write_frame};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
#[path = "lease.rs"]
pub mod lease;

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum AttachRequest {
    AnswerQuestion {
        agent_id: String,
        question_id: String,
        /// One per question of the form, in any order, each question exactly
        /// once.
        answers: Vec<crate::question::QuestionAnswer>,
        /// The client asserts it showed the "always" confirmation naming the
        /// option's scope. The daemon cannot verify it; it refuses an "always"
        /// option without it, so a client that skipped the dialog has to claim
        /// otherwise explicitly rather than by omission.
        #[serde(default)]
        confirmed_always: bool,
        /// PRD #1631 (audit A4): the revision of the question the answer was
        /// worked out against ([`crate::question::PendingQuestion::revision`]
        /// on the snapshot). The daemon refuses the answer as stale unless it
        /// is the revision pending now, so a question replaced under the same
        /// id between the client's read and the request is never answered with
        /// a choice made for the old one. A request that omits it is refused
        /// the same way whenever the pending question has a revision (PR #1698
        /// review); no released client sends this request, so none relied on
        /// the id check alone. Optional on the wire only so that a pending
        /// question the daemon stamped no revision on can still be named.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<u64>,
    },
}

pub struct Client {
    pub socket: PathBuf,
    pub sent: Arc<Mutex<Vec<Value>>>,
}

async fn exchange(rd: &mut OwnedReadHalf, wr: &mut OwnedWriteHalf, request: &Value) -> Result<AttachResponse, ClientError> {
    write_frame(wr, KIND_REQ, &serde_json::to_vec(request).unwrap()).await?;
    let Some((kind, bytes)) = read_frame(rd).await? else {
        return Err(ClientError::Server("closed response".into()));
    };
    assert_eq!(kind, KIND_RESP);
    Ok(serde_json::from_slice(&bytes).unwrap())
}
async fn issue_command(rd: &mut OwnedReadHalf, wr: &mut OwnedWriteHalf, request: &AttachRequest) -> Result<AttachResponse, ClientError> {
    exchange(rd, wr, &serde_json::to_value(request).unwrap()).await
}
impl Client {
    async fn capabilities(&self) -> Result<DaemonCapabilities, ClientError> {
        dot_agent_deck::daemon_client::DaemonClient::new(self.socket.clone()).capabilities().await
    }
    async fn admitted_connect(&self) -> Result<(OwnedReadHalf, OwnedWriteHalf), ClientError> {
        let stream = tokio::net::UnixStream::connect(&self.socket).await?;
        let (mut rd, mut wr) = stream.into_split();
        let hello = exchange(&mut rd, &mut wr, &serde_json::json!({"op": "hello", "client_version": dot_agent_deck::daemon_protocol::PROTOCOL_VERSION, "follow_on": true})).await?;
        assert!(hello.ok, "released-client transport Hello: {hello:?}");
        Ok((rd, wr))
    }
    pub async fn answer_question_while(
        &self,
        still_wanted: impl Fn() -> bool,
        agent_id: &str,
        question_id: &str,
        revision: Option<u64>,
        answers: Vec<crate::question::QuestionAnswer>,
        confirmed_always: bool,
    ) -> Result<AnswerReport, ClientError> {
        if !self
            .capabilities()
            .await?
            .supports(crate::daemon_protocol::CAP_ANSWER_QUESTION)
        {
            return Ok(AnswerReport::Withheld);
        }
        // Admitted first, then `still_wanted`: the wait for the gate's `Hello`
        // is part of the window a cancelled answer must still be dropped in.
        let (mut rd, mut wr) = self.admitted_connect().await?;
        if !still_wanted() {
            return Ok(AnswerReport::Superseded);
        }
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::AnswerQuestion {
                agent_id: agent_id.to_string(),
                question_id: question_id.to_string(),
                answers,
                confirmed_always,
                revision,
            },
        )
        .await?;
        if resp.ok {
            return Ok(AnswerReport::Answered);
        }
        match resp.answer_refusal {
            Some(refusal) => Ok(AnswerReport::Refused(refusal)),
            None => Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "answer-question failed".into()),
            )),
        }
    }


}
const VOICE_ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct AnswerOutcome { pub kind: &'static str, pub code: Option<&'static str>, pub sentence: String }
pub fn unconfirmed_outcome(agent: &str, code: Option<&'static str>) -> AnswerOutcome {
    AnswerOutcome {
        kind: "unconfirmed",
        code,
        sentence: format!(
            "The answer may have been sent, but {agent} did not confirm it — check {agent} before answering again."
        ),
    }
}

pub fn timeout_outcome(leased: lease::Leased, agent: &str) -> AnswerOutcome {
    use lease::Leased;
    match leased {
        Leased::Unconfirmed(_) => unconfirmed_outcome(agent, None),

        other => panic!("expected released timeout, got {other:?}"),
    }
}
pub fn progress_words(agent: &str) -> (&'static str, String) {
    match dot_agent_deck::question::AnswerRefusal::AnswerInProgress {
        AnswerRefusal::AnswerInProgress => (
            "answer_in_progress",
            format!("Another answer to {agent} is still being sent — nothing was sent."),
        ),

        _ => unreachable!(),
    }
}
fn safe_message(value: String) -> String { value }

impl Client {
    pub async fn voice_answer(&self, agent_id: &str, question_id: &str, revision: Option<u64>) -> lease::Leased {
        let agent_id = agent_id.to_owned();
        let question_id = question_id.to_owned();
        let client = self;
        let confirmed_always = false;
        let answers = vec![crate::question::QuestionAnswer { question_index: 0, option_indices: vec![1], text: None }];
        // Record exactly the frozen wire shape before the send. This is a fixture
        // observation, not a replacement for the daemon or terminal assertions.
        self.sent.lock().unwrap().push(serde_json::to_value(AttachRequest::AnswerQuestion {
            agent_id: agent_id.clone(), question_id: question_id.clone(), revision,
            answers: answers.clone(), confirmed_always,
        }).unwrap());
        let leases = lease::AnswerLeases::default();
        let lease = leases.begin("1");
        let refused = Mutex::new(None::<String>);
        lease::send_leased(&lease, |still_wanted| async {
        let sent = client.answer_question_while(
            still_wanted,
            &agent_id,
            &question_id,
            revision,
            answers,
            confirmed_always,
        );
        match tokio::time::timeout(VOICE_ANSWER_TIMEOUT, sent).await {
            Ok(Err(
                error @ (dot_agent_deck::daemon_client::ClientError::DaemonRestarted
                | dot_agent_deck::daemon_client::ClientError::DaemonUnnamed),
            )) => {
                let sentence = error.to_string();
                *refused.lock().unwrap_or_else(|p| p.into_inner()) = Some(sentence.clone());
                Err(sentence)
            }
            Ok(report) => report.map_err(|error| safe_message(error.to_string())),
            Err(_) => Err("the deck did not answer in time".to_string()),
        }

        }).await.unwrap()
    }
}
