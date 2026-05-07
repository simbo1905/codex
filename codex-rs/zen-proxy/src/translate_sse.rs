//! Translate Anthropic streaming SSE into OAI Responses streaming SSE.
//!
//! Implements `Read` so it can be passed directly as a `tiny_http::Response` body.
//! Internally drives a synchronous state machine that consumes upstream SSE lines
//! from a `reqwest::blocking::Response` and emits translated OAI events.
//!
//! ## Event mapping
//!
//! ```text
//! Anthropic                          OAI Responses
//! ─────────────────────────────────  ────────────────────────────────────────────
//! message_start                   →  response.created + response.in_progress
//! content_block_start (text)      →  response.output_item.added +
//!                                    response.content_part.added
//! content_block_start (tool_use)  →  response.output_item.added (function_call)
//! content_block_delta (text)      →  response.output_text.delta
//! content_block_delta (json)      →  response.function_call_arguments.delta
//! content_block_stop  (text)      →  response.output_text.done +
//!                                    response.content_part.done +
//!                                    response.output_item.done
//! content_block_stop  (tool_use)  →  response.function_call_arguments.done +
//!                                    response.output_item.done
//! message_stop                    →  response.completed
//! ```

use std::collections::VecDeque;
use std::io;
use std::io::Read;
use std::time;

use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

/// Per-content-block state during translation.
struct BlockState {
    block_type: BlockType,
    /// For text blocks: accumulated text so far.
    text: String,
    /// For tool_use blocks: accumulated JSON arguments.
    json_acc: String,
    /// Stable unique ID used in OAI events.
    oai_id: String,
    /// Tool name (tool_use only).
    tool_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockType {
    Text,
    ToolUse,
}

/// A `Read` implementation that translates Anthropic SSE into OAI Responses SSE on the fly.
pub(crate) struct AnthropicToOaiStream {
    model: String,
    resp_id: String,
    created_at: u64,
    seq: u64,
    /// Content blocks indexed by Anthropic block index.
    blocks: Vec<Option<BlockState>>,
    /// Whether we've emitted `response.created` / `response.in_progress`.
    opened: bool,
    /// Buffered translated bytes not yet consumed by `read()`.
    buf: VecDeque<u8>,
    /// Upstream response (used as a line source).
    upstream: reqwest::blocking::Response,
    /// Whether the upstream stream is exhausted.
    done: bool,
    /// Residual bytes from the last upstream read (partial SSE line).
    line_buf: Vec<u8>,
}

impl AnthropicToOaiStream {
    pub(crate) fn new(model: String, upstream: reqwest::blocking::Response) -> Self {
        let resp_id = format!("resp_{}", Uuid::new_v4().simple());
        let created_at = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            model,
            resp_id,
            created_at,
            seq: 0,
            blocks: Vec::new(),
            opened: false,
            buf: VecDeque::new(),
            upstream,
            done: false,
            line_buf: Vec::new(),
        }
    }
}

impl Read for AnthropicToOaiStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Keep filling the translation buffer until we have data to return
        // or the upstream stream is exhausted.
        loop {
            if !self.buf.is_empty() {
                let n = self.buf.len().min(out.len());
                for (dst, src) in out[..n].iter_mut().zip(self.buf.drain(..n)) {
                    *dst = src;
                }
                return Ok(n);
            }

            if self.done {
                return Ok(0);
            }

            // Pull one SSE line from the upstream.
            match self.next_line() {
                Ok(Some(line)) => self.process_line(&line),
                Ok(None) => {
                    self.done = true;
                    return Ok(0);
                }
                Err(e) => return Err(io::Error::other(e.to_string())),
            }
        }
    }
}

impl AnthropicToOaiStream {
    /// Read the next newline-terminated line from the upstream response.
    fn next_line(&mut self) -> anyhow::Result<Option<String>> {
        let mut tmp = [0u8; 4096];
        loop {
            // Check if line_buf already contains a newline.
            if let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = self.line_buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes).trim_end().to_string();
                return Ok(Some(line));
            }
            // Read more bytes from upstream.
            let n = self.upstream.read(&mut tmp)?;
            if n == 0 {
                // EOF: flush remainder as final line if non-empty.
                if !self.line_buf.is_empty() {
                    let line = String::from_utf8_lossy(&self.line_buf)
                        .trim_end()
                        .to_string();
                    self.line_buf.clear();
                    return Ok(Some(line));
                }
                return Ok(None);
            }
            self.line_buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Parse one SSE line and push translated OAI events into `self.buf`.
    fn process_line(&mut self, line: &str) {
        if !line.starts_with("data:") {
            return;
        }
        let payload = line["data:".len()..].trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let ev: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("zen-proxy: non-JSON anthropic SSE: {e} — {payload}");
                return;
            }
        };

        match ev["type"].as_str() {
            Some("message_start") => self.on_message_start(),
            Some("content_block_start") => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                self.on_content_block_start(idx, &ev["content_block"]);
            }
            Some("content_block_delta") => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                self.on_content_block_delta(idx, &ev["delta"]);
            }
            Some("content_block_stop") => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                self.on_content_block_stop(idx);
            }
            Some("message_delta") => {
                // Capture usage — stored on message_stop for the completed event.
            }
            Some("message_stop") => self.on_message_stop(&ev),
            Some("ping") => {} // silently ignored
            Some(other) => eprintln!("zen-proxy: unhandled anthropic event type: {other}"),
            None => {}
        }
    }

    // ── event handlers ────────────────────────────────────────────────────────

    fn on_message_start(&mut self) {
        if self.opened {
            return;
        }
        self.opened = true;
        let skeleton = self.skeleton("in_progress");
        self.emit("response.created", skeleton.clone());
        self.emit("response.in_progress", skeleton);
        self.seq = 2;
    }

    fn on_content_block_start(&mut self, idx: usize, block: &Value) {
        let btype_str = block["type"].as_str().unwrap_or("text");
        match btype_str {
            "text" => {
                let oai_id = format!("msg_{}", Uuid::new_v4().simple());
                let state = BlockState {
                    block_type: BlockType::Text,
                    text: String::new(),
                    json_acc: String::new(),
                    oai_id: oai_id.clone(),
                    tool_name: String::new(),
                };
                self.set_block(idx, state);

                self.emit_with_seq(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": idx,
                        "item": {
                            "id": oai_id,
                            "type": "message",
                            "status": "in_progress",
                            "role": "assistant",
                            "content": [],
                        },
                    }),
                );
                self.emit_with_seq(
                    "response.content_part.added",
                    json!({
                        "type": "response.content_part.added",
                        "output_index": idx,
                        "content_index": 0,
                        "item_id": oai_id,
                        "part": {"type": "output_text", "text": "", "annotations": []},
                    }),
                );
            }
            "tool_use" => {
                let call_id = block["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                let tool_name = block["name"].as_str().unwrap_or("").to_string();
                let state = BlockState {
                    block_type: BlockType::ToolUse,
                    text: String::new(),
                    json_acc: String::new(),
                    oai_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                };
                self.set_block(idx, state);

                self.emit_with_seq(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": idx,
                        "item": {
                            "id": call_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "name": tool_name,
                            "arguments": "",
                            "call_id": call_id,
                        },
                    }),
                );
            }
            other => eprintln!("zen-proxy: unknown content_block type: {other}"),
        }
    }

    fn on_content_block_delta(&mut self, idx: usize, delta: &Value) {
        let dtype = delta["type"].as_str().unwrap_or("");
        match dtype {
            "text_delta" => {
                let text = delta["text"].as_str().unwrap_or("").to_string();
                let (oai_id, seq) = if let Some(Some(block)) = self.blocks.get_mut(idx) {
                    block.text.push_str(&text);
                    (block.oai_id.clone(), self.seq)
                } else {
                    return;
                };
                self.emit_with_seq(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "output_index": idx,
                        "content_index": 0,
                        "item_id": oai_id,
                        "delta": text,
                        "sequence_number": seq,
                    }),
                );
            }
            "input_json_delta" => {
                let chunk = delta["partial_json"].as_str().unwrap_or("").to_string();
                let (call_id, seq) = if let Some(Some(block)) = self.blocks.get_mut(idx) {
                    block.json_acc.push_str(&chunk);
                    (block.oai_id.clone(), self.seq)
                } else {
                    return;
                };
                self.emit_with_seq(
                    "response.function_call_arguments.delta",
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": idx,
                        "item_id": call_id,
                        "delta": chunk,
                        "sequence_number": seq,
                    }),
                );
            }
            other => eprintln!("zen-proxy: unknown content_block_delta type: {other}"),
        }
    }

    fn on_content_block_stop(&mut self, idx: usize) {
        let state = match self.blocks.get(idx) {
            Some(Some(s)) => BlockState {
                block_type: s.block_type,
                text: s.text.clone(),
                json_acc: s.json_acc.clone(),
                oai_id: s.oai_id.clone(),
                tool_name: s.tool_name.clone(),
            },
            _ => return,
        };

        match state.block_type {
            BlockType::Text => {
                self.emit_with_seq(
                    "response.output_text.done",
                    json!({
                        "type": "response.output_text.done",
                        "output_index": idx,
                        "content_index": 0,
                        "item_id": state.oai_id,
                        "text": state.text,
                    }),
                );
                self.emit_with_seq(
                    "response.content_part.done",
                    json!({
                        "type": "response.content_part.done",
                        "output_index": idx,
                        "content_index": 0,
                        "item_id": state.oai_id,
                        "part": {"type": "output_text", "text": state.text, "annotations": []},
                    }),
                );
                self.emit_with_seq("response.output_item.done", json!({
                    "type": "response.output_item.done",
                    "output_index": idx,
                    "item": {
                        "id": state.oai_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": state.text, "annotations": []}],
                    },
                }));
            }
            BlockType::ToolUse => {
                self.emit_with_seq(
                    "response.function_call_arguments.done",
                    json!({
                        "type": "response.function_call_arguments.done",
                        "output_index": idx,
                        "item_id": state.oai_id,
                        "arguments": state.json_acc,
                    }),
                );
                self.emit_with_seq(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": idx,
                        "item": {
                            "id": state.oai_id,
                            "type": "function_call",
                            "status": "completed",
                            "name": state.tool_name,
                            "arguments": state.json_acc,
                            "call_id": state.oai_id,
                        },
                    }),
                );
            }
        }
    }

    fn on_message_stop(&mut self, _ev: &Value) {
        // Build the output array from all closed blocks.
        let mut output: Vec<Value> = Vec::new();
        for block_opt in &self.blocks {
            let Some(block) = block_opt else { continue };
            match block.block_type {
                BlockType::Text => {
                    output.push(json!({
                        "id": block.oai_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": block.text, "annotations": []}],
                    }));
                }
                BlockType::ToolUse => {
                    output.push(json!({
                        "id": block.oai_id,
                        "type": "function_call",
                        "status": "completed",
                        "name": block.tool_name,
                        "arguments": block.json_acc,
                        "call_id": block.oai_id,
                    }));
                }
            }
        }

        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": self.resp_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "completed",
                "model": self.model,
                "output": output,
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens": 0,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 0,
                },
                "error": null,
            },
        });
        self.emit("response.completed", completed);
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn skeleton(&self, status: &str) -> Value {
        json!({
            "id": self.resp_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": [],
            "usage": null,
            "error": null,
        })
    }

    fn emit(&mut self, event_type: &str, data: Value) {
        let line = format!("event: {event_type}\ndata: {data}\n\n");
        self.buf.extend(line.as_bytes());
    }

    fn emit_with_seq(&mut self, event_type: &str, mut data: Value) {
        let seq = self.seq;
        self.seq += 1;
        if let Value::Object(ref mut map) = data {
            map.insert("sequence_number".to_string(), json!(seq));
        }
        self.emit(event_type, data);
    }

    fn set_block(&mut self, idx: usize, state: BlockState) {
        if idx >= self.blocks.len() {
            self.blocks.resize_with(idx + 1, || None);
        }
        self.blocks[idx] = Some(state);
    }
}
