use std::{
    io::{self, Write},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use synapse_action::{ActionHandle, RELEASE_ALL_HANDLE, install_panic_hook};
use synapse_core::{Action, error_codes};
use tracing_subscriber::fmt::writer::MakeWriter;

#[test]
fn panic_hook_timeout_keeps_release_all_in_queue_and_preserves_previous_hook() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let _trace_guard = tracing::subscriber::set_default(subscriber);

    let (handle, mut action_rx) = ActionHandle::channel();
    assert!(
        RELEASE_ALL_HANDLE.set(handle).is_ok(),
        "RELEASE_ALL_HANDLE should be unset at integration-test process start"
    );

    let previous_count = Arc::new(AtomicUsize::new(0));
    let previous_count_for_hook = Arc::clone(&previous_count);
    panic::set_hook(Box::new(move |_info| {
        previous_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    println!(
        "readback=panic_hook_queue edge=timeout before=queued:{} previous_count:{}",
        action_rx.len(),
        previous_count.load(Ordering::SeqCst)
    );

    install_panic_hook();
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        panic!("synthetic #180 timeout panic");
    }));
    assert!(result.is_err());
    assert_eq!(previous_count.load(Ordering::SeqCst), 1);
    assert_eq!(action_rx.len(), 1);

    let log_output = trace_buffer.text();
    println!("readback=panic_hook_log edge=timeout raw={log_output:?}");
    let log_line = find_log_line(&log_output, error_codes::SAFETY_RELEASE_ALL_FIRED);
    assert!(log_line.contains("reason=\"panic\""));
    assert!(log_line.contains("timeout_ms=10"));
    assert!(log_line.contains("result=\"error\""));
    assert!(log_line.contains("timed out after 10ms"));

    let action_label = match action_rx.try_recv() {
        Ok((Action::ReleaseAll, _ack, _operator_panic_epoch_at_enqueue)) => "release_all",
        Ok((_action, _ack, _operator_panic_epoch_at_enqueue)) => "unexpected",
        Err(error) => panic!("release_all action should remain queued after timeout: {error:?}"),
    };
    assert_eq!(action_label, "release_all");

    println!(
        "readback=panic_hook_queue edge=timeout after_queued_before_drain:1 after_drained_action:{action_label} previous_count:{} panic_caught:{} log_line={log_line}",
        previous_count.load(Ordering::SeqCst),
        result.is_err()
    );
}

fn find_log_line(log_output: &str, needle: &str) -> String {
    log_output
        .lines()
        .find(|line| line.contains(needle))
        .map_or_else(
            || panic!("expected log output to contain {needle}, got {log_output:?}"),
            ToOwned::to_owned,
        )
}

#[derive(Clone, Default)]
struct SharedTraceBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTraceBuffer {
    fn text(&self) -> String {
        let bytes = match self.bytes.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl<'a> MakeWriter<'a> for SharedTraceBuffer {
    type Writer = SharedTraceBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedTraceBufferWriter {
            bytes: Arc::clone(&self.bytes),
        }
    }
}

struct SharedTraceBufferWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedTraceBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.bytes.lock() {
            Ok(mut guard) => guard.extend_from_slice(buf),
            Err(poisoned) => poisoned.into_inner().extend_from_slice(buf),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
