//! #1091–#1095 FSV: drive the REAL `console_capture_*` engine against a live
//! Chrome on a synthetic page with KNOWN console output, and prove every
//! captured entry matches what the page actually emitted.
//!
//! Source of Truth: the live `Runtime.consoleAPICalled` / `Runtime.exceptionThrown`
//! / `Log.entryAdded` event stream — captured into the engine's per-target ring
//! buffer, then read back and compared to the synthetic inputs we KNOW the page
//! produced (X+X=Y: we control the logs, we assert the captured outputs).
//!
//! Usage (a Chromium must be listening with --remote-debugging-port):
//!   cargo run -p synapse-a11y --example cdp_console_fsv -- http://127.0.0.1:9333
#![allow(clippy::expect_used, clippy::too_many_lines, clippy::print_stdout)]

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use std::time::Duration;

    use chromiumoxide::Browser;
    use futures_util::StreamExt as _;
    use synapse_a11y::{
        ConsoleReadFilter, DEFAULT_CONSOLE_BUFFER_CAPACITY, console_capture_ensure,
        console_capture_read, console_capture_stop,
    };

    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:9333".to_owned());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");

    let mut pass = 0u32;
    let mut fail = 0u32;

    rt.block_on(async move {
        let (browser, mut handler) = Browser::connect(&endpoint).await.expect("connect to Chrome");
        let _drive = tokio::spawn(async move { while handler.next().await.is_some() {} });

        // --- Page A: the primary capture target -----------------------------
        let page = browser.new_page("about:blank").await.expect("new page");
        let target_id = page.target_id().inner().clone();
        page.evaluate("document.title='Synapse Console FSV'")
            .await
            .expect("title");

        // ARM capture BEFORE emitting — Chrome does not replay console history.
        let status = console_capture_ensure(&endpoint, &target_id, DEFAULT_CONSOLE_BUFFER_CAPACITY)
            .await
            .expect("arm capture");
        println!(
            "readback=arm stage=setup target={target_id} newly_armed={} armed_at={}",
            status.newly_armed, status.armed_at_unix_ms
        );
        assert!(status.newly_armed, "first ensure must newly arm");

        // Give the capture connection a moment to finish Runtime/Log enable +
        // event-listener registration before the page emits.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // === Emit a KNOWN set of console output =========================
        page.evaluate(
            r#"
            console.log('syn-log-line');
            console.info('syn-info-line');
            console.warn('syn-warn-line');
            console.error('syn-error-line');
            console.debug('syn-debug-line');
            console.log('syn-object', {a: 1, ok: true, name: 'synapse'});
            console.log('syn-array', [10, 20, 'z']);
            console.log('syn-multi', 1, true, null);
            "#,
        )
        .await
        .expect("emit console");

        // Uncaught exception (async so evaluate doesn't swallow it) + unhandled
        // promise rejection — both surface via Runtime.exceptionThrown.
        // Defer BOTH via setTimeout so this evaluate() returns cleanly (the
        // errors must fire AFTER evaluate so they reach the capture connection's
        // Runtime.exceptionThrown stream, not this command's own response).
        page.evaluate(
            r#"
            setTimeout(() => { throw new Error('syn-uncaught-boom'); }, 0);
            setTimeout(() => { Promise.reject(new Error('syn-rejected-promise')); }, 0);
            "#,
        )
        .await
        .expect("emit errors");

        // Let the async events propagate to the capture connection.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let all = console_capture_read(&target_id, &ConsoleReadFilter { max: 1000, ..Default::default() })
            .expect("buffer armed");
        println!(
            "readback=read stage=all returned={} total_buffered={} dropped={} next_cursor={}",
            all.returned, all.total_buffered, all.dropped, all.next_cursor
        );
        for e in &all.entries {
            println!(
                "  seq={} source={} level={} text={:?} args={} url={:?} line={:?} stack={}",
                e.seq,
                e.source,
                e.level,
                e.text,
                serde_json::to_string(&e.args).unwrap_or_default(),
                e.url,
                e.line,
                e.stack.is_some()
            );
        }

        macro_rules! check {
            ($label:expr, $cond:expr) => {{
                if $cond {
                    pass += 1;
                    println!("PASS  {}", $label);
                } else {
                    fail += 1;
                    println!("FAIL  {}", $label);
                }
            }};
        }

        let find = |source: &str, text_needle: &str| {
            all.entries
                .iter()
                .find(|e| e.source == source && e.text.contains(text_needle))
                .cloned()
        };

        // --- Happy path: each console level captured with correct level ----
        for (needle, level) in [
            ("syn-log-line", "log"),
            ("syn-info-line", "info"),
            ("syn-warn-line", "warning"),
            ("syn-error-line", "error"),
            ("syn-debug-line", "debug"),
        ] {
            let entry = find("console-api", needle);
            check!(
                format!("console.{level} captured ({needle})"),
                entry.as_ref().is_some_and(|e| e.level == level && e.text.contains(needle))
            );
        }

        // --- Object arg is structured JSON, never [object Object] ----------
        let obj = find("console-api", "syn-object");
        check!(
            "object arg reconstructed to structured JSON",
            obj.as_ref().is_some_and(|e| {
                e.args.get(1) == Some(&serde_json::json!({"a": 1, "ok": true, "name": "synapse"}))
                    && !e.text.contains("[object Object]")
            })
        );
        let arr = find("console-api", "syn-array");
        check!(
            "array arg reconstructed to JSON array",
            arr.as_ref()
                .is_some_and(|e| e.args.get(1) == Some(&serde_json::json!([10, 20, "z"])))
        );
        let multi = find("console-api", "syn-multi");
        check!(
            "multi-arg primitives preserved (1 true null)",
            multi.as_ref().is_some_and(|e| {
                e.args == vec![
                    serde_json::json!("syn-multi"),
                    serde_json::json!(1),
                    serde_json::json!(true),
                    serde_json::json!(null),
                ]
            })
        );

        // --- Edge case: uncaught throw → page-error WITH a stack -----------
        let boom = find("page-error", "syn-uncaught-boom");
        check!(
            "uncaught throw captured as page-error with message",
            boom.as_ref().is_some_and(|e| e.level == "error" && e.text.contains("syn-uncaught-boom"))
        );
        check!(
            "uncaught throw carries a stack trace",
            boom.as_ref().is_some_and(|e| e.stack.is_some())
        );

        // --- Edge case: unhandled rejection recorded DISTINCTLY ------------
        let rej = find("unhandled-rejection", "syn-rejected-promise");
        check!(
            "unhandled rejection captured distinctly from console.error",
            rej.is_some()
        );
        check!(
            "rejection source != console-api and != page-error",
            rej.as_ref().is_some_and(|e| e.source == "unhandled-rejection")
        );

        // --- Filter: level=error narrows to error-level only --------------
        let only_err = console_capture_read(
            &target_id,
            &ConsoleReadFilter { level: Some("error"), max: 1000, ..Default::default() },
        )
        .expect("buffer");
        check!(
            "level=error filter returns only error-level entries",
            only_err.returned > 0 && only_err.entries.iter().all(|e| e.level == "error")
        );

        // --- Filter: source=page-error narrows to page errors -------------
        let only_pageerr = console_capture_read(
            &target_id,
            &ConsoleReadFilter { source: Some("page-error"), max: 1000, ..Default::default() },
        )
        .expect("buffer");
        check!(
            "source=page-error filter excludes console + rejection",
            only_pageerr.returned >= 1 && only_pageerr.entries.iter().all(|e| e.source == "page-error")
        );

        // --- Delta cursor: since_seq returns only newer entries -----------
        let cursor = all.next_cursor;
        page.evaluate("console.log('syn-after-cursor')").await.expect("emit");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let delta = console_capture_read(
            &target_id,
            &ConsoleReadFilter { since_seq: Some(cursor), max: 1000, ..Default::default() },
        )
        .expect("buffer");
        check!(
            "since_seq cursor returns ONLY the new entry",
            delta.returned == 1 && delta.entries[0].text.contains("syn-after-cursor")
        );
        check!(
            "delta entry seq is at or after the cursor",
            delta.entries.first().is_some_and(|e| e.seq >= cursor)
        );

        // --- Per-target isolation: Page B logs do NOT leak into Page A -----
        let page_b = browser.new_page("about:blank").await.expect("page B");
        let target_b = page_b.target_id().inner().clone();
        console_capture_ensure(&endpoint, &target_b, DEFAULT_CONSOLE_BUFFER_CAPACITY)
            .await
            .expect("arm B");
        tokio::time::sleep(Duration::from_millis(300)).await;
        page_b.evaluate("console.log('syn-PAGE-B-ONLY')").await.expect("emit B");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let a_after = console_capture_read(&target_id, &ConsoleReadFilter { max: 1000, ..Default::default() })
            .expect("A");
        let b_after = console_capture_read(&target_b, &ConsoleReadFilter { max: 1000, ..Default::default() })
            .expect("B");
        check!(
            "Page B's log appears in Page B's buffer",
            b_after.entries.iter().any(|e| e.text.contains("syn-PAGE-B-ONLY"))
        );
        check!(
            "Page B's log does NOT leak into Page A's buffer (per-target isolation)",
            !a_after.entries.iter().any(|e| e.text.contains("syn-PAGE-B-ONLY"))
        );

        // --- Idempotent re-arm reuses the live capture (no duplication) ----
        let rearm = console_capture_ensure(&endpoint, &target_id, DEFAULT_CONSOLE_BUFFER_CAPACITY)
            .await
            .expect("re-arm");
        check!("idempotent ensure reuses live capture (newly_armed=false)", !rearm.newly_armed);

        // --- Teardown stops capture -----------------------------------------
        let stopped = console_capture_stop(&target_id);
        check!("stop tears down the capture", stopped);
        check!(
            "read after stop returns None (not armed)",
            console_capture_read(&target_id, &ConsoleReadFilter::default()).is_none()
        );
        console_capture_stop(&target_b);

        println!("\nFSV RESULT: {pass} passed, {fail} failed");
        if fail > 0 {
            std::process::exit(1);
        }
    });
}
