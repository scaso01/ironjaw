// B2 CP3 emit-site payload tests.
//
// These tests don't spin up a full `AppState`; they validate the *payload
// shape* each emit site in handler.rs constructs by mirroring the same
// `HookEvent` construction in isolation against a real `HookRegistry`. This
// catches drift between the handler's emit and any downstream consumer
// expecting a specific payload, without the cost of building a full
// connection context.

#![cfg(test)]

use std::sync::Arc;

use super::{HookAction, HookCallback, HookContext, HookEvent, HookEventKind, HookRegistry};

/// Helper using std::sync::Mutex (sync-safe inside the closure).
fn install_recorder_sync(reg_capture: Arc<std::sync::Mutex<Vec<HookContext>>>) -> HookCallback {
    Arc::new(move |ctx| {
        if let Ok(mut g) = reg_capture.lock() {
            g.push(ctx.clone());
        }
        HookAction::Continue
    })
}

#[tokio::test]
async fn emit_user_prompt_submit_payload_shape() {
    let reg = HookRegistry::new();
    let captured: Arc<std::sync::Mutex<Vec<HookContext>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    reg.register_fn(
        HookEventKind::UserPromptSubmit,
        "recorder",
        0,
        install_recorder_sync(captured.clone()),
    )
    .await;

    // Mirror exactly what handler.rs handle_chat_send constructs.
    let session_id = "sess-123".to_string();
    let prompt_text = "hello world".to_string();
    let model = "anthropic/claude-sonnet-4-6".to_string();
    let force_fresh = true;

    let event = HookEvent::UserPromptSubmit {
        session_id: session_id.clone(),
        prompt_text: prompt_text.clone(),
        model: model.clone(),
        force_fresh,
    };
    let ctx = HookContext::with_session(
        event,
        &session_id,
        serde_json::json!({"model": &model, "force_fresh": force_fresh}),
    );
    let action = reg.fire(&ctx).await;
    assert!(action.is_continue());

    let buf = captured.lock().unwrap();
    assert_eq!(buf.len(), 1);
    let fired = &buf[0];
    assert_eq!(fired.session_id.as_deref(), Some("sess-123"));
    if let HookEvent::UserPromptSubmit {
        session_id: sid,
        prompt_text: pt,
        model: m,
        force_fresh: ff,
    } = &fired.event
    {
        assert_eq!(sid, "sess-123");
        assert_eq!(pt, "hello world");
        assert_eq!(m, "anthropic/claude-sonnet-4-6");
        assert!(*ff);
    } else {
        panic!("expected UserPromptSubmit, got {:?}", fired.event);
    }
}

#[tokio::test]
async fn emit_user_prompt_submit_block_action_propagates() {
    let reg = HookRegistry::new();
    reg.register_fn(
        HookEventKind::UserPromptSubmit,
        "blocker",
        0,
        Arc::new(|_| HookAction::Block("policy violation".into())),
    )
    .await;

    let event = HookEvent::UserPromptSubmit {
        session_id: "s".into(),
        prompt_text: "p".into(),
        model: "m".into(),
        force_fresh: false,
    };
    let ctx = HookContext::with_session(event, "s", serde_json::json!(null));
    let action = reg.fire(&ctx).await;
    assert!(action.is_block());
    if let HookAction::Block(reason) = action {
        assert_eq!(reason, "policy violation");
    } else {
        unreachable!();
    }
}

#[tokio::test]
async fn emit_pre_compact_payload_shape() {
    let reg = HookRegistry::new();
    let captured: Arc<std::sync::Mutex<Vec<HookContext>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    reg.register_fn(
        HookEventKind::PreCompact,
        "recorder",
        0,
        install_recorder_sync(captured.clone()),
    )
    .await;

    // Mirror the chat.send post-stream PreCompact emit.
    let session_id = "sess-abc".to_string();
    let message_count = 42usize;
    let total_tokens = 8000u64;

    let event = HookEvent::PreCompact {
        session_id: session_id.clone(),
        message_count,
        total_tokens,
    };
    let ctx = HookContext::with_session(
        event,
        &session_id,
        serde_json::json!({
            "site": "chat.send",
            "message_count": message_count,
            "total_tokens": total_tokens,
        }),
    );
    assert!(reg.fire(&ctx).await.is_continue());
    let buf = captured.lock().unwrap();
    assert_eq!(buf.len(), 1);
    if let HookEvent::PreCompact {
        session_id: sid,
        message_count: mc,
        total_tokens: tt,
    } = &buf[0].event
    {
        assert_eq!(sid, "sess-abc");
        assert_eq!(*mc, 42);
        assert_eq!(*tt, 8000);
    } else {
        panic!("expected PreCompact");
    }
    // The data payload also carries the site marker so a hook can distinguish
    // between chat.send-triggered compaction and chat.compact RPC compaction.
    assert_eq!(
        buf[0].data.get("site").and_then(|v| v.as_str()),
        Some("chat.send")
    );
}

#[tokio::test]
async fn emit_pre_compact_block_skips_compaction_intent() {
    // Mirror the explicit chat.compact handler's Block path: when a hook
    // returns Block, the handler returns COMPACT_BLOCKED instead of running
    // compact_history. We assert only the action propagation here; the
    // handler-level frame shape is covered by an integration test in a
    // future wave (out of scope for B2 unit tests).
    let reg = HookRegistry::new();
    reg.register_fn(
        HookEventKind::PreCompact,
        "guard",
        0,
        Arc::new(|_| HookAction::Block("snapshot pending".into())),
    )
    .await;
    let event = HookEvent::PreCompact {
        session_id: "s".into(),
        message_count: 1,
        total_tokens: 1,
    };
    let ctx = HookContext::with_session(event, "s", serde_json::json!(null));
    let action = reg.fire(&ctx).await;
    assert!(action.is_block());
}

#[tokio::test]
async fn emit_skill_activated_payload_shape() {
    let reg = HookRegistry::new();
    let captured: Arc<std::sync::Mutex<Vec<HookContext>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    reg.register_fn(
        HookEventKind::SkillActivated,
        "recorder",
        0,
        install_recorder_sync(captured.clone()),
    )
    .await;

    // Mirror handle_role_switch's SkillActivated emit.
    let session_id = "sess-xyz".to_string();
    let event = HookEvent::SkillActivated {
        session_id: session_id.clone(),
        skill_name: "coder".into(),
        prior_skill: Some("research".into()),
    };
    let ctx = HookContext::with_session(
        event,
        &session_id,
        serde_json::json!({"site": "role.switch", "prior": "research", "next": "coder"}),
    );
    assert!(reg.fire(&ctx).await.is_continue());
    let buf = captured.lock().unwrap();
    assert_eq!(buf.len(), 1);
    if let HookEvent::SkillActivated {
        session_id: sid,
        skill_name: sn,
        prior_skill: ps,
    } = &buf[0].event
    {
        assert_eq!(sid, "sess-xyz");
        assert_eq!(sn, "coder");
        assert_eq!(ps.as_deref(), Some("research"));
    } else {
        panic!("expected SkillActivated");
    }
}

#[tokio::test]
async fn emit_skill_deactivated_payload_shape() {
    let reg = HookRegistry::new();
    let captured: Arc<std::sync::Mutex<Vec<HookContext>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    reg.register_fn(
        HookEventKind::SkillDeactivated,
        "recorder",
        0,
        install_recorder_sync(captured.clone()),
    )
    .await;

    let session_id = "sess-xyz".to_string();
    let event = HookEvent::SkillDeactivated {
        session_id: session_id.clone(),
        skill_name: "research".into(),
        next_skill: Some("coder".into()),
    };
    let ctx = HookContext::with_session(
        event,
        &session_id,
        serde_json::json!({"site": "role.switch", "prior": "research", "next": "coder"}),
    );
    assert!(reg.fire(&ctx).await.is_continue());
    let buf = captured.lock().unwrap();
    assert_eq!(buf.len(), 1);
    if let HookEvent::SkillDeactivated {
        session_id: sid,
        skill_name: sn,
        next_skill: ns,
    } = &buf[0].event
    {
        assert_eq!(sid, "sess-xyz");
        assert_eq!(sn, "research");
        assert_eq!(ns.as_deref(), Some("coder"));
    } else {
        panic!("expected SkillDeactivated");
    }
}

#[tokio::test]
async fn emit_skill_pair_around_role_switch_fires_in_order() {
    // role.switch fires SkillDeactivated(prior) THEN SkillActivated(new).
    // Verify both are visible and in order via a single shared buffer.
    let reg = HookRegistry::new();
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let dcap = captured.clone();
    let dcb: HookCallback = Arc::new(move |ctx| {
        if let HookEvent::SkillDeactivated { skill_name, .. } = &ctx.event {
            dcap.lock().unwrap().push(format!("D:{skill_name}"));
        }
        HookAction::Continue
    });
    reg.register_fn(HookEventKind::SkillDeactivated, "rec", 0, dcb)
        .await;

    let acap = captured.clone();
    let acb: HookCallback = Arc::new(move |ctx| {
        if let HookEvent::SkillActivated { skill_name, .. } = &ctx.event {
            acap.lock().unwrap().push(format!("A:{skill_name}"));
        }
        HookAction::Continue
    });
    reg.register_fn(HookEventKind::SkillActivated, "rec", 0, acb)
        .await;

    // Mirror handler.rs role.switch order: deactivate prior, then activate new.
    let dctx = HookContext::with_session(
        HookEvent::SkillDeactivated {
            session_id: "s".into(),
            skill_name: "old".into(),
            next_skill: Some("new".into()),
        },
        "s",
        serde_json::json!(null),
    );
    let _ = reg.fire(&dctx).await;

    let actx = HookContext::with_session(
        HookEvent::SkillActivated {
            session_id: "s".into(),
            skill_name: "new".into(),
            prior_skill: Some("old".into()),
        },
        "s",
        serde_json::json!(null),
    );
    let _ = reg.fire(&actx).await;

    let buf = captured.lock().unwrap();
    assert_eq!(buf.as_slice(), &["D:old".to_string(), "A:new".to_string()]);
}
