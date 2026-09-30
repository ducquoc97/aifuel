//! Attachment handling: images ride `image` content blocks only when
//! the agent advertised the capability, everything else degrades to
//! `resource_link`, which every ACP agent accepts.

use super::*;
use aifuel_core::{AgentAdapter, Attachment, AttachmentKind};

/// The prompt's content blocks for one send, extracted from the wire.
async fn prompt_blocks(agent: &mut FakeAgent) -> Vec<Value> {
    let prompt = agent.next_method("session/prompt").await;
    agent
        .respond(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    prompt["params"]["prompt"]
        .as_array()
        .cloned()
        .expect("the prompt carries content blocks")
}

#[test]
fn an_image_attachment_sends_an_image_block_when_advertised() {
    let workspace = temp_workspace("img");
    let image = workspace.join("pixel.png");
    std::fs::write(&image, [0x89, 0x50, 0x4e, 0x47]).expect("write");
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .handshake_with("sess-1", json!({"promptCapabilities": {"image": true}}))
            .await;
        let blocks = prompt_blocks(&mut agent).await;
        let image = blocks
            .iter()
            .find(|block| block["type"] == "image")
            .expect("an image block rides the prompt");
        assert_eq!(image["mimeType"], "image/png");
        assert_eq!(image["data"], "iVBORw==", "the bytes are base64");
        assert!(
            image["uri"]
                .as_str()
                .unwrap_or_default()
                .starts_with("file://")
        );
        agent.park().await;
    });
    let handle = adapter
        .start(
            &integration(),
            options_at(workspace.clone(), AccessMode::WorkspaceWrite),
        )
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let mut message = input("look at this");
    message.attachments.push(Attachment {
        kind: AttachmentKind::Image,
        path: image,
    });
    adapter.send(&handle, message).expect("send");
    let _ = through_idle(&events);
    adapter.stop(handle).expect("stop");
    let _ = std::fs::remove_dir_all(&workspace);
    script.join().expect("the agent script completes");
}

#[test]
fn an_image_without_the_capability_degrades_to_a_resource_link() {
    let workspace = temp_workspace("img-nocap");
    let image = workspace.join("pixel.png");
    std::fs::write(&image, [0x89, 0x50, 0x4e, 0x47]).expect("write");
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .handshake_with("sess-1", json!({"promptCapabilities": {"image": false}}))
            .await;
        let blocks = prompt_blocks(&mut agent).await;
        assert!(
            blocks.iter().all(|block| block["type"] != "image"),
            "no image block when the capability is unadvertised"
        );
        let link = blocks
            .iter()
            .find(|block| block["type"] == "resource_link")
            .expect("the image degrades to a resource link");
        assert_eq!(link["name"], "pixel.png");
        agent.park().await;
    });
    let handle = adapter
        .start(
            &integration(),
            options_at(workspace.clone(), AccessMode::WorkspaceWrite),
        )
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let mut message = input("look at this");
    message.attachments.push(Attachment {
        kind: AttachmentKind::Image,
        path: image,
    });
    adapter.send(&handle, message).expect("send");
    let _ = through_idle(&events);
    adapter.stop(handle).expect("stop");
    let _ = std::fs::remove_dir_all(&workspace);
    script.join().expect("the agent script completes");
}

#[test]
fn a_file_attachment_sends_a_resource_link() {
    let workspace = temp_workspace("file");
    let file = workspace.join("notes.txt");
    std::fs::write(&file, "some notes").expect("write");
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let blocks = prompt_blocks(&mut agent).await;
        let link = blocks
            .iter()
            .find(|block| block["type"] == "resource_link")
            .expect("a file attachment rides a resource link");
        assert_eq!(link["name"], "notes.txt");
        assert_eq!(link["size"], 10);
        agent.park().await;
    });
    let handle = adapter
        .start(
            &integration(),
            options_at(workspace.clone(), AccessMode::WorkspaceWrite),
        )
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let mut message = input("read these");
    message.attachments.push(Attachment {
        kind: AttachmentKind::File,
        path: file,
    });
    adapter.send(&handle, message).expect("send");
    let _ = through_idle(&events);
    adapter.stop(handle).expect("stop");
    let _ = std::fs::remove_dir_all(&workspace);
    script.join().expect("the agent script completes");
}
