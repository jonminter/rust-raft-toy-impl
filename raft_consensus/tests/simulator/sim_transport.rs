use std::{sync::mpsc, thread, time::Duration};

use raft_consensus::{
    rpc_messages::{ReplyTo, Request, RpcMessage},
    system_clock,
    transport::RaftTransportBridge,
};
use tracing::trace;

use super::common::{ClockAdvance, SimLogCommand};

/// Transport used by raft nodes in the simulator. Allows the simulated network to send/receive messages from the raft nodes.
/// Parks the Raft node's thread when it is waiting for the next message, and unparks it when the simulator clock is updated
/// so that it can check if the wait timeout has been reached.
pub(crate) struct SimNetworkRaftTransport {
    outbound_message_tx: mpsc::Sender<RpcMessage<SimLogCommand>>,
    inbound_message_rx: mpsc::Receiver<RpcMessage<SimLogCommand>>,
    timer_tx: mpsc::Sender<ClockAdvance>,
    thread_handle: Option<thread::Thread>,
}
impl SimNetworkRaftTransport {
    pub(crate) fn new(
        outbound_message_tx: mpsc::Sender<RpcMessage<SimLogCommand>>,
        inbound_message_rx: mpsc::Receiver<RpcMessage<SimLogCommand>>,
        timer_tx: mpsc::Sender<ClockAdvance>,
    ) -> Self {
        Self {
            outbound_message_tx,
            inbound_message_rx,
            timer_tx,
            thread_handle: None,
        }
    }
}

impl RaftTransportBridge<SimLogCommand> for SimNetworkRaftTransport {
    fn wait_for_next_incoming_message(
        &mut self,
        max_wait: Duration,
    ) -> Option<RpcMessage<SimLogCommand>> {
        let current_thread = thread::current();
        let current_thread_id = current_thread.id();
        let saved_handle = self.thread_handle.get_or_insert(current_thread);

        assert_eq!(
            saved_handle.id(),
            current_thread_id,
            "Simulated network transport can only be used from a single thread"
        );

        let started_waiting_at = system_clock::now();

        self.timer_tx
            .send(ClockAdvance(max_wait))
            .expect("Could not queue timer advance request to simulator");

        loop {
            trace!("Simulated network transport checking for incoming messages...");
            if let Ok(message) = self.inbound_message_rx.try_recv() {
                return Some(message);
            } else {
                let time_waited = system_clock::now() - started_waiting_at;
                if time_waited >= max_wait {
                    return None;
                }
                // info!("PARKING THREAD: {:?}", current_thread_id);
                thread::park();
            }
        }
    }

    fn enqueue_outgoing_request(&mut self, request: Request<SimLogCommand>) {
        self.outbound_message_tx
            .send(RpcMessage::Request(request))
            .expect("Could not queue outbound message to simulated network");
    }

    fn enqueue_reply(&mut self, reply: ReplyTo) {
        self.outbound_message_tx
            .send(RpcMessage::Reply(reply))
            .expect("Could not queue outbound message to simulated network");
    }
}

mod tests {
    use std::thread;
    use test_log::test;

    use mock_instant::MockClock;
    use std::time::Duration;

    use raft_consensus::{
        rpc_messages::{ReplyTo, RpcMessage, Vote},
        transport::RaftTransportBridge,
        ServerId, TermIndex,
    };

    #[test]
    fn sim_transport_should_be_send() {
        fn assert_send<T: Send>() {}
        assert_send::<super::SimNetworkRaftTransport>();
    }

    #[test]
    fn sim_transport_should_receive_message() {
        let (outbound_tx, _) = std::sync::mpsc::channel();
        let (inbound_tx, inbound_rx) = std::sync::mpsc::channel();
        let (timer_tx, _timer_rx) = std::sync::mpsc::channel();

        let mut transport = super::SimNetworkRaftTransport::new(outbound_tx, inbound_rx, timer_tx);

        let thread_handle = std::thread::spawn(move || {
            transport
                .wait_for_next_incoming_message(Duration::from_millis(127))
                .unwrap()
        });

        let reply = ReplyTo::RequestVote(Vote {
            request_id: uuid::Uuid::new_v4(),
            from: ServerId(1),
            to: ServerId(2),
            term: TermIndex(1),
            vote_granted: true,
        });

        let expected_message = RpcMessage::Reply(reply.clone());

        let message = RpcMessage::Reply(reply);

        inbound_tx.send(message).unwrap();

        let received_message = thread_handle.join().unwrap();

        assert_eq!(expected_message, received_message);
    }

    #[test]
    fn sim_transport_should_timeout_waiting_for_next_message() {
        let (outbound_tx, _) = std::sync::mpsc::channel();
        let (_, inbound_rx) = std::sync::mpsc::channel();
        let (timer_tx, _timer_rx) = std::sync::mpsc::channel();

        let mut transport = super::SimNetworkRaftTransport::new(outbound_tx, inbound_rx, timer_tx);

        let thread_handle = std::thread::spawn(move || {
            let message = transport.wait_for_next_incoming_message(Duration::from_millis(127));
            message.is_none()
        });

        // Wait for the thread to park itself (TODO - is there a better way to do this?)
        thread::sleep(Duration::from_millis(10));

        // Should park itself again since the clock hasn't changed
        thread_handle.thread().unpark();
        // Note: There is no guarantee the thread was actually unparked
        // before this assertion, so passing this doesn't neccessarily mean
        // it works, but failing this would mean it definitely does not work properly
        assert!(!thread_handle.is_finished());

        // Now if we advance the clock, it should timeout when it is unparked
        MockClock::advance(Duration::from_millis(128));
        thread_handle.thread().unpark();

        assert_eq!(true, thread_handle.join().unwrap());
    }
}