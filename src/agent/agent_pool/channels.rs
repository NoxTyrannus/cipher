use super::super::communication::AgentMessage;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub turn_id: String,
    pub reason: String,
    /// v0.4.9 P2 退出关断：true = 关断信号（trigger 任务收到后 `break` 自然退出）。
    pub shutdown: bool,
}

impl TriggerEvent {
    pub fn round(turn_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            reason: reason.into(),
            shutdown: false,
        }
    }

    pub fn shutdown() -> Self {
        Self {
            turn_id: String::new(),
            reason: "shutdown".to_string(),
            shutdown: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessageBus {
    pub execution_tx: mpsc::Sender<AgentMessage>,

    pub insight_tx: mpsc::Sender<AgentMessage>,

    pub memory_tx: mpsc::Sender<AgentMessage>,

    pub trigger_tx: mpsc::Sender<TriggerEvent>,
}

impl MessageBus {
    pub fn send_to_execution(&self, msg: AgentMessage) -> Result<(), AgentMessage> {
        self.execution_tx.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(m) => m,
            mpsc::error::TrySendError::Closed(m) => m,
        })
    }

    pub fn send_to_insight(&self, msg: AgentMessage) -> Result<(), AgentMessage> {
        self.insight_tx.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(m) => m,
            mpsc::error::TrySendError::Closed(m) => m,
        })
    }

    pub fn send_to_memory(&self, msg: AgentMessage) -> Result<(), AgentMessage> {
        self.memory_tx.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(m) => m,
            mpsc::error::TrySendError::Closed(m) => m,
        })
    }

    pub fn send_trigger(&self, event: TriggerEvent) -> Result<(), TriggerEvent> {
        self.trigger_tx.try_send(event).map_err(|e| match e {
            mpsc::error::TrySendError::Full(e) => e,
            mpsc::error::TrySendError::Closed(e) => e,
        })
    }

    pub async fn send_to_execution_backpressure(&self, msg: AgentMessage) -> Result<(), String> {
        send_with_timeout(&self.execution_tx, msg, "execution").await
    }

    pub async fn send_to_insight_backpressure(&self, msg: AgentMessage) -> Result<(), String> {
        send_with_timeout(&self.insight_tx, msg, "insight").await
    }

    pub async fn send_to_memory_backpressure(&self, msg: AgentMessage) -> Result<(), String> {
        send_with_timeout(&self.memory_tx, msg, "memory").await
    }

    pub async fn send_trigger_backpressure(&self, event: TriggerEvent) -> Result<(), String> {
        tokio::time::timeout(SEND_BACKPRESSURE_TIMEOUT, self.trigger_tx.send(event))
            .await
            .map_err(|_| "send timeout: trigger channel overloaded/stuck".to_string())?
            .map_err(|e| format!("send closed: {e}"))
    }
}

const SEND_BACKPRESSURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn send_with_timeout(
    tx: &mpsc::Sender<AgentMessage>,
    msg: AgentMessage,
    channel: &str,
) -> Result<(), String> {
    tokio::time::timeout(SEND_BACKPRESSURE_TIMEOUT, tx.send(msg))
        .await
        .map_err(|_| format!("send timeout: {channel} channel overloaded/stuck"))?
        .map_err(|e| format!("send closed: {e}"))
}

pub struct MessageReceivers {
    pub execution_rx: mpsc::Receiver<AgentMessage>,
    pub insight_rx: mpsc::Receiver<AgentMessage>,
    pub memory_rx: mpsc::Receiver<AgentMessage>,

    pub trigger_rx: mpsc::Receiver<TriggerEvent>,
}

pub fn create_message_bus() -> (MessageBus, MessageReceivers) {
    let (execution_tx, execution_rx) = mpsc::channel(64);
    let (insight_tx, insight_rx) = mpsc::channel(64);
    let (memory_tx, memory_rx) = mpsc::channel(64);
    let (trigger_tx, trigger_rx) = mpsc::channel(64);

    let bus = MessageBus {
        execution_tx,
        insight_tx,
        memory_tx,
        trigger_tx,
    };
    let receivers = MessageReceivers {
        execution_rx,
        insight_rx,
        memory_rx,
        trigger_rx,
    };
    (bus, receivers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_bus_and_send_message() {
        let (bus, mut receivers) = create_message_bus();

        let msg = AgentMessage::Execute {
            turn_id: "t1".into(),
        };
        bus.send_to_execution(msg).unwrap();

        let received = receivers.execution_rx.try_recv();
        assert!(received.is_ok());
    }

    #[test]
    fn message_routes_to_correct_receiver() {
        let (bus, mut receivers) = create_message_bus();

        bus.send_to_execution(AgentMessage::Execute {
            turn_id: "t1".into(),
        })
        .unwrap();
        bus.send_to_insight(AgentMessage::ExecutionDone {
            turn_id: "t2".into(),
        })
        .unwrap();
        bus.send_to_memory(AgentMessage::InsightDone {
            turn_id: "t3".into(),
        })
        .unwrap();

        assert!(receivers.execution_rx.try_recv().is_ok());
        assert!(receivers.insight_rx.try_recv().is_ok());
        assert!(receivers.memory_rx.try_recv().is_ok());
    }
}
