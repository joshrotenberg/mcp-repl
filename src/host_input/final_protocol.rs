use std::future::poll_fn;
use std::task::Poll;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;
use tower_mcp::client::{McpClient, TaskAwareCallToolOutcome};
use tower_mcp::protocol::{
    CallToolResult, InputRequest, InputRequests, InputResponse, InputResponses,
    PROTOCOL_VERSION_2026_07_28, TaskObject, TaskStatus,
};

use super::{
    Connection, HOST, Host, Question, Withdrawal, elicit_result, sample_result, transport_error,
};

const MAX_ATTEMPTS: usize = 4;
const DISCONNECT_POLL: Duration = Duration::from_millis(100);

pub(crate) async fn call_tool(
    client: &McpClient,
    name: &str,
    arguments: Value,
) -> tower_mcp::Result<CallToolResult> {
    let Some(host) = HOST.get() else {
        return client.call_tool(name, arguments).await;
    };
    if client.selected_protocol_version().await.as_deref() != Some(PROTOCOL_VERSION_2026_07_28) {
        return client.call_tool(name, arguments).await;
    }
    let connection = host
        .connection()
        .ok_or_else(|| transport_error("host input connection unavailable"))?;
    let deadline = crate::request_timeout().map(|limit| Instant::now() + limit);
    let mut responses = None;
    let mut state = None;
    for attempt in 1..=MAX_ATTEMPTS {
        if host.lost.is_cancelled() {
            return Err(super::lost_error());
        }
        if !client.is_connected() {
            return Err(transport_error("MCP connection closed during host input"));
        }
        let outcome = client
            .call_tool_once_task_aware(name, arguments.clone(), responses.take(), state.take())
            .await?;
        match outcome {
            TaskAwareCallToolOutcome::Complete(result) => return Ok(result),
            TaskAwareCallToolOutcome::Task(created) => {
                // Preserve the existing task wait path. Its automatic input
                // resolution uses the regular fallback handler, never the
                // bridge, because there is no stable cancellation token.
                let id = &created.task.metadata.task_id;
                return task_result(client.task_wait(id).await?);
            }
            TaskAwareCallToolOutcome::InputRequired(required) => {
                required.validate().map_err(transport_error)?;
                if attempt == MAX_ATTEMPTS {
                    return Err(transport_error("host input round limit exceeded"));
                }
                responses = match required.input_requests {
                    Some(requests) if !requests.is_empty() => {
                        Some(answer_round(client, host, connection, requests, deadline).await?)
                    }
                    _ => None,
                };
                state = required.request_state;
            }
            _ => return Err(transport_error("unsupported tool call outcome")),
        }
    }
    unreachable!("the last attempt completes or returns an error")
}

fn task_result(task: TaskObject) -> tower_mcp::Result<CallToolResult> {
    match task.status {
        TaskStatus::Completed => task.result.ok_or_else(|| {
            tower_mcp::Error::Transport(format!(
                "completed task '{}' did not contain a result",
                task.task_id
            ))
        }),
        TaskStatus::Failed => Err(tower_mcp::Error::JsonRpc(task.error.unwrap_or_else(|| {
            tower_mcp::error::JsonRpcError::internal_error(format!(
                "task '{}' failed without an error payload",
                task.task_id
            ))
        }))),
        TaskStatus::Cancelled => Err(tower_mcp::Error::Transport(format!(
            "task '{}' was cancelled",
            task.task_id
        ))),
        _ => Err(transport_error("task did not reach a terminal state")),
    }
}

async fn answer_round(
    client: &McpClient,
    host: &Host,
    connection: Connection,
    requests: InputRequests,
    deadline: Option<Instant>,
) -> tower_mcp::Result<InputResponses> {
    if requests.len() > super::MAX_PENDING {
        return Err(transport_error(
            "host input batch exceeds the pending limit",
        ));
    }
    let mut replies = InputResponses::new();
    let mut questions = Vec::new();
    for (key, request) in requests {
        let (question, sampling) = match request {
            InputRequest::Elicit(params) => (Question::elicit(params), false),
            InputRequest::CreateMessage(params) => {
                (Some(Question::Sampling(Box::new(params))), true)
            }
            undeclared => {
                return Err(tower_mcp::Error::Transport(format!(
                    "server requested undeclared MRTR input capability: {}",
                    undeclared.method_name()
                )));
            }
        };
        if let Some(question) = question {
            questions.push(((key, sampling), question));
        } else {
            replies.insert(
                key,
                InputResponse::Elicit(tower_mcp::protocol::ElicitResult::decline()),
            );
        }
    }
    if questions.is_empty() {
        return Ok(replies);
    }
    let mut tickets = host
        .open(connection, questions, None, deadline)
        .map_err(|reason| match reason {
            Withdrawal::HostGone => super::lost_error(),
            Withdrawal::Expired => transport_error("host input deadline expired"),
            Withdrawal::Disconnected => transport_error("MCP connection closed during host input"),
            _ => transport_error("host input round unavailable"),
        })?;
    let Some((_, first)) = tickets.first() else {
        return Ok(replies);
    };
    let expiration = tokio::time::sleep_until(first.deadline);
    tokio::pin!(expiration);
    let disconnect = async {
        while client.is_connected() {
            tokio::time::sleep(DISCONNECT_POLL).await;
        }
    };
    tokio::pin!(disconnect);
    let mut expired = false;
    while !tickets.is_empty() {
        let (index, answer) = tokio::select! {
            biased;
            () = host.lost.cancelled() => return Err(super::lost_error()),
            () = &mut disconnect => {
                for (_, ticket) in &tickets {
                    host.withdraw(ticket.id, ticket.connection, Withdrawal::Disconnected);
                }
                return Err(transport_error("MCP connection closed during host input"));
            }
            () = &mut expiration, if !expired => {
                expired = true;
                for (_, ticket) in &tickets {
                    host.withdraw(ticket.id, ticket.connection, Withdrawal::Expired);
                }
                continue;
            }
            result = poll_fn(|cx| {
                for (index, (_, ticket)) in tickets.iter_mut().enumerate() {
                    if let Poll::Ready(answer) = std::pin::Pin::new(&mut ticket.receiver).poll(cx) {
                        return Poll::Ready((index, answer.unwrap_or(Err(Withdrawal::Cancelled))));
                    }
                }
                Poll::Pending
            }) => result,
        };
        let ((key, sampling), _ticket) = tickets.swap_remove(index);
        match &answer {
            Err(Withdrawal::Disconnected) => {
                return Err(transport_error("MCP connection closed during host input"));
            }
            Err(Withdrawal::HostGone) => return Err(super::lost_error()),
            _ => {}
        }
        let reply = if sampling {
            InputResponse::CreateMessage(sample_result(answer).map_err(tower_mcp::Error::JsonRpc)?)
        } else {
            InputResponse::Elicit(elicit_result(answer))
        };
        replies.insert(key, reply);
    }
    Ok(replies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_mcp::{ChannelTransport, McpRouter, protocol::ListRootsParams};

    #[tokio::test]
    async fn roots_requires_a_declared_capability() {
        let (host, mut frames, connection) = super::super::tests::host();
        let client = McpClient::connect(ChannelTransport::new(McpRouter::new()))
            .await
            .unwrap();
        let inputs = [(
            "roots".into(),
            InputRequest::ListRoots(ListRootsParams::default()),
        )]
        .into_iter()
        .collect();
        let error = answer_round(&client, &host, connection, inputs, None)
            .await
            .unwrap_err();
        assert!(matches!(error, tower_mcp::Error::Transport(message)
            if message == "server requested undeclared MRTR input capability: roots/list"));
        assert!(frames.try_recv().is_err());
        client.shutdown().await.unwrap();
    }
}
