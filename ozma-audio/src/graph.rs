// SPDX-License-Identifier: AGPL-3.0-only
//! Read-only enumeration of an existing PipeWire/PulseAudio graph.
//!
//! The graph is read through the PulseAudio compatibility API that
//! `pipewire-pulse` provides: `pactl -f json list ...` and `pactl -f json
//! info`. Nothing here creates, moves, unloads, or mutes a node. A missing
//! `pactl`, an unreachable server, or output this crate cannot parse is a typed
//! [`AudioError`], never an empty fabricated graph.
//!
//! Only a small, fixed subset of each object is copied: index, name,
//! description, media class, state, mute, volume, and monitor metadata. The
//! raw PulseAudio property bag is never serialized because it can contain
//! application names, file names, and other user data. The number of nodes and
//! streams is bounded by [`MAX_GRAPH_NODES`] and [`MAX_GRAPH_STREAMS`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AudioError;
use crate::router::{check_status, render_command, CommandOutput, CommandRunner, PACTL};

/// Largest number of sinks and sources returned in one snapshot.
pub const MAX_GRAPH_NODES: usize = 256;
/// Largest number of sink-inputs and source-outputs returned in one snapshot.
pub const MAX_GRAPH_STREAMS: usize = 512;
/// Bound for one `pactl -f json` invocation.
pub const GRAPH_TIMEOUT: Duration = Duration::from_secs(5);

/// One sink or source from the audio graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioGraphNode {
    /// PulseAudio object index; the dashboard's `id`.
    pub id: u64,
    /// Object name used by `pactl`, `pw-play`, and `parec`.
    pub name: String,
    /// Human description, falling back to the name.
    pub description: String,
    /// `Audio/Sink` or `Audio/Source`.
    pub media_class: String,
    /// Whether the server reports this as the default sink/source.
    pub default: bool,
    /// Server state, for example `RUNNING` or `SUSPENDED`.
    pub state: String,
    /// Whether the sink/source is muted.
    pub mute: bool,
    /// Average channel volume in percent, when reported.
    pub volume_percent: Option<f64>,
    /// The `<sink>.monitor` source for a sink.
    pub monitor_source: Option<String>,
    /// The sink this source monitors, for a monitor source.
    pub monitor_of_sink: Option<String>,
}

/// One active sink-input or source-output stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioGraphStream {
    /// PulseAudio stream index.
    pub index: u64,
    /// `sink-input` or `source-output`.
    pub kind: String,
    /// Application name when the server reports one; a generic label otherwise.
    pub application: String,
    /// Sink or source name the stream is connected to.
    pub target: Option<String>,
    /// Whether the stream is muted.
    pub mute: bool,
    /// Whether a sink-input is corked.
    pub corked: bool,
}

/// A bounded, read-only view of the audio graph.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioGraphSnapshot {
    /// Sinks then sources, capped at [`MAX_GRAPH_NODES`].
    pub nodes: Vec<AudioGraphNode>,
    /// Active sink-input/source-output streams.
    #[serde(default)]
    pub streams: Vec<AudioGraphStream>,
    /// Graph links are not exposed by `pactl`; this is always empty.
    #[serde(default)]
    pub links: Vec<AudioGraphLink>,
    /// The server's default sink, when it reports one.
    pub default_sink: Option<String>,
    /// The server's default source, when it reports one.
    pub default_source: Option<String>,
    /// True when a list was shortened to a bound.
    pub truncated: bool,
    /// False because `pactl` has no graph-link listing. The empty
    /// [`AudioGraphSnapshot::links`] is absence, not an empty graph.
    pub links_available: bool,
}

/// A graph link. `pactl` does not expose these, so the list stays empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioGraphLink {
    /// Upstream node name.
    pub from: String,
    /// Downstream node name.
    pub to: String,
}

/// Read one bounded snapshot through `pactl -f json`.
///
/// The five commands are independent: the first failure is returned. They are
/// read-only (`list`/`info`), so a concurrent graph change can only produce a
/// slightly inconsistent snapshot, never a mutation.
pub async fn read_graph(runner: &dyn CommandRunner) -> Result<AudioGraphSnapshot, AudioError> {
    let info = pactl_json(runner, &["-f", "json", "info"]).await?;
    let sinks = pactl_json(runner, &["-f", "json", "list", "sinks"]).await?;
    let sources = pactl_json(runner, &["-f", "json", "list", "sources"]).await?;
    let sink_inputs = pactl_json(runner, &["-f", "json", "list", "sink-inputs"]).await?;
    let source_outputs = pactl_json(runner, &["-f", "json", "list", "source-outputs"]).await?;

    let default_sink = info
        .get("default_sink_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let default_source = info
        .get("default_source_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);

    let sink_values = json_array(&sinks, "list sinks")?;
    let source_values = json_array(&sources, "list sources")?;
    let sink_input_values = json_array(&sink_inputs, "list sink-inputs")?;
    let source_output_values = json_array(&source_outputs, "list source-outputs")?;

    let mut truncated = false;
    let mut nodes = Vec::new();
    let mut sink_names = std::collections::HashMap::new();

    for value in sink_values.iter().take(MAX_GRAPH_NODES) {
        let node = parse_node(value, "Audio/Sink", default_sink.as_deref(), "list sinks")?;
        if let Some(name) = value.get("monitor_source_name").and_then(Value::as_str) {
            if !name.trim().is_empty() {
                sink_names.insert(node.id, name.to_owned());
            }
        }
        sink_names.insert(node.id, node.name.clone());
        nodes.push(node);
    }
    if sink_values.len() > MAX_GRAPH_NODES {
        truncated = true;
    }

    let remaining = MAX_GRAPH_NODES.saturating_sub(nodes.len());
    for value in source_values.iter().take(remaining) {
        nodes.push(parse_node(
            value,
            "Audio/Source",
            default_source.as_deref(),
            "list sources",
        )?);
    }
    if source_values.len() > remaining {
        truncated = true;
    }

    let mut source_names = std::collections::HashMap::new();
    for value in source_values.iter().take(MAX_GRAPH_STREAMS) {
        if let (Some(index), Some(name)) = (
            value.get("index").and_then(Value::as_u64),
            value.get("name").and_then(Value::as_str),
        ) {
            source_names.insert(index, name.to_owned());
        }
    }

    let mut streams = Vec::new();
    for value in sink_input_values.iter().take(MAX_GRAPH_STREAMS) {
        streams.push(parse_stream(
            value,
            "sink-input",
            "sink",
            Some(&sink_names),
            "list sink-inputs",
        )?);
    }
    if sink_input_values.len() > MAX_GRAPH_STREAMS {
        truncated = true;
    }
    let remaining = MAX_GRAPH_STREAMS.saturating_sub(streams.len());
    for value in source_output_values.iter().take(remaining) {
        streams.push(parse_stream(
            value,
            "source-output",
            "source",
            Some(&source_names),
            "list source-outputs",
        )?);
    }
    if source_output_values.len() > remaining {
        truncated = true;
    }

    Ok(AudioGraphSnapshot {
        nodes,
        streams,
        links: Vec::new(),
        default_sink,
        default_source,
        truncated,
        links_available: false,
    })
}

async fn pactl_json(runner: &dyn CommandRunner, args: &[&str]) -> Result<Value, AudioError> {
    let argv = std::iter::once(PACTL.to_owned())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect::<Vec<_>>();
    let output = tokio::time::timeout(GRAPH_TIMEOUT, runner.run(&argv, None))
        .await
        .map_err(|_| AudioError::Process {
            command: render_command(&argv),
            message: format!("timed out after {}s", GRAPH_TIMEOUT.as_secs()),
        })??;
    let output = check_status(&argv, output)?;
    serde_json::from_str(&output.stdout).map_err(|error| AudioError::OutputParse {
        command: render_command(&argv),
        output: bounded_output(&output),
        reason: format!("expected JSON: {error}"),
    })
}

fn json_array<'a>(value: &'a Value, operation: &str) -> Result<&'a Vec<Value>, AudioError> {
    value.as_array().ok_or_else(|| AudioError::OutputParse {
        command: format!("{PACTL} -f json {operation}"),
        output: bounded_output(&CommandOutput::new(0, value.to_string(), "")),
        reason: "expected a JSON array".to_owned(),
    })
}

fn parse_node(
    value: &Value,
    media_class: &str,
    default_name: Option<&str>,
    operation: &str,
) -> Result<AudioGraphNode, AudioError> {
    let index = value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| parse_error(operation, value, "missing numeric `index`"))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| parse_error(operation, value, "missing `name`"))?
        .to_owned();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .unwrap_or(&name)
        .to_owned();
    let monitor_source = value
        .get("monitor_source_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_owned);
    let monitor_of_sink = value
        .get("monitor_of_sink")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sink| !sink.is_empty() && *sink != "n/a")
        .map(str::to_owned);

    Ok(AudioGraphNode {
        id: index,
        name: name.clone(),
        description,
        media_class: media_class.to_owned(),
        default: default_name.map(|default| default == name).unwrap_or(false),
        state: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_owned(),
        mute: value.get("mute").and_then(Value::as_bool).unwrap_or(false),
        volume_percent: value.get("volume").and_then(volume_percent),
        monitor_source,
        monitor_of_sink,
    })
}

fn parse_stream(
    value: &Value,
    kind: &str,
    target_field: &str,
    names: Option<&std::collections::HashMap<u64, String>>,
    operation: &str,
) -> Result<AudioGraphStream, AudioError> {
    let index = value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| parse_error(operation, value, "missing numeric `index`"))?;
    let target = value
        .get(target_field)
        .and_then(Value::as_u64)
        .and_then(|id| {
            names
                .and_then(|names| names.get(&id).cloned())
                .or_else(|| Some(id.to_string()))
        });
    let application = value
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("application.name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 80)
        .unwrap_or("stream")
        .to_owned();

    Ok(AudioGraphStream {
        index,
        kind: kind.to_owned(),
        application,
        target,
        mute: value.get("mute").and_then(Value::as_bool).unwrap_or(false),
        corked: value
            .get("corked")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn volume_percent(volume: &Value) -> Option<f64> {
    let channels = volume.as_object()?;
    let mut total = 0.0;
    let mut count = 0u32;
    for channel in channels.values() {
        if let Some(percent) = channel
            .get("value_percent")
            .and_then(Value::as_str)
            .and_then(|percent| percent.trim_end_matches('%').trim().parse::<f64>().ok())
        {
            total += percent;
            count += 1;
            continue;
        }
        if let Some(raw) = channel.get("value").and_then(Value::as_f64) {
            total += raw / 65_536.0 * 100.0;
            count += 1;
        }
    }
    (count > 0).then(|| ((total / f64::from(count)) * 10.0).round() / 10.0)
}

fn parse_error(operation: &str, value: &Value, reason: &str) -> AudioError {
    AudioError::OutputParse {
        command: format!("{PACTL} -f json {operation}"),
        output: value.to_string().chars().take(512).collect(),
        reason: reason.to_owned(),
    }
}

fn bounded_output(output: &CommandOutput) -> String {
    output.stdout.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct FakeRunner {
        responses: std::sync::Mutex<
            std::collections::VecDeque<(String, Result<CommandOutput, AudioError>)>,
        >,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            }
        }

        fn push_json(self, needle: &str, body: &str) -> Self {
            self.responses
                .lock()
                .unwrap()
                .push_back((needle.to_owned(), Ok(CommandOutput::success(body))));
            self
        }

        fn push_error(self, needle: &str, error: AudioError) -> Self {
            self.responses
                .lock()
                .unwrap()
                .push_back((needle.to_owned(), Err(error)));
            self
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            argv: &[String],
            _stdin: Option<&str>,
        ) -> Result<CommandOutput, AudioError> {
            let rendered = render_command(argv);
            let mut responses = self.responses.lock().unwrap();
            if let Some(position) = responses
                .iter()
                .position(|(needle, _)| rendered.contains(needle.as_str()))
            {
                return responses.remove(position).unwrap().1;
            }
            panic!("unexpected command: {rendered}");
        }

        async fn spawn(&self, _argv: &[String]) -> Result<crate::router::ChildId, AudioError> {
            unreachable!()
        }

        async fn kill(&self, _child: crate::router::ChildId) -> Result<(), AudioError> {
            unreachable!()
        }
    }

    fn sample_graph() -> FakeRunner {
        FakeRunner::new()
            .push_json(
                "json info",
                r#"{"default_sink_name":"speakers","default_source_name":"mic","cookie":"secret"}"#,
            )
            .push_json(
                "json list sinks",
                r#"[{"index":1,"state":"RUNNING","name":"speakers","description":"Speakers","monitor_source_name":"speakers.monitor","mute":false,"volume":{"front-left":{"value_percent":"40%"},"front-right":{"value_percent":"40%"}},"properties":{"application.name":"private"}}]"#,
            )
            .push_json(
                "json list sources",
                r#"[{"index":2,"state":"IDLE","name":"mic","description":"Microphone","mute":true,"volume":{"mono":{"value_percent":"70%"}},"properties":{}},{"index":3,"state":"IDLE","name":"speakers.monitor","description":"Monitor of Speakers","monitor_of_sink":"speakers","mute":false,"volume":{}}]"#,
            )
            .push_json(
                "json list sink-inputs",
                r#"[{"index":9,"sink":1,"mute":false,"corked":true,"properties":{"application.name":"Browser","media.name":"/home/user/secret.txt"}}]"#,
            )
            .push_json("json list source-outputs", "[]")
    }

    #[tokio::test]
    async fn read_graph_classifies_nodes_defaults_and_bounded_streams() {
        let runner = sample_graph();
        let graph = read_graph(&runner).await.unwrap();

        assert_eq!(graph.default_sink.as_deref(), Some("speakers"));
        assert_eq!(graph.default_source.as_deref(), Some("mic"));
        assert_eq!(graph.nodes.len(), 3);
        assert!(graph.links.is_empty());
        assert!(!graph.links_available);
        assert!(!graph.truncated);

        let sink = &graph.nodes[0];
        assert_eq!(sink.id, 1);
        assert_eq!(sink.media_class, "Audio/Sink");
        assert_eq!(sink.description, "Speakers");
        assert!(sink.default);
        assert_eq!(sink.monitor_source.as_deref(), Some("speakers.monitor"));
        assert_eq!(sink.volume_percent, Some(40.0));

        assert!(graph.nodes[1].default);
        assert!(graph.nodes[1].mute);
        assert_eq!(graph.nodes[1].volume_percent, Some(70.0));

        let monitor = &graph.nodes[2];
        assert!(!monitor.default);
        assert_eq!(monitor.monitor_of_sink.as_deref(), Some("speakers"));

        assert_eq!(graph.streams.len(), 1);
        assert_eq!(graph.streams[0].target.as_deref(), Some("speakers"));
        assert_eq!(graph.streams[0].application, "Browser");
        assert!(graph.streams[0].corked);

        // The raw property bag (including the media file name) is never copied.
        let json = serde_json::to_string(&graph).unwrap();
        assert!(!json.contains("secret.txt"));
        assert!(!json.contains("cookie"));
        assert!(!json.contains("application.name"));
    }

    #[tokio::test]
    async fn read_graph_propagates_tool_failure_as_a_typed_error() {
        let runner = FakeRunner::new().push_error(
            "json info",
            AudioError::MissingCommand {
                command: PACTL.to_owned(),
                remediation: "install pactl".to_owned(),
            },
        );
        let error = read_graph(&runner).await.unwrap_err();
        assert!(matches!(error, AudioError::MissingCommand { .. }));
    }

    #[tokio::test]
    async fn read_graph_rejects_malformed_json() {
        let runner = FakeRunner::new().push_json("json info", "not json");
        let error = read_graph(&runner).await.unwrap_err();
        assert!(matches!(error, AudioError::OutputParse { .. }));
    }

    #[test]
    fn volume_percent_accepts_raw_values_and_rejects_empty_maps() {
        assert_eq!(
            volume_percent(&serde_json::json!({"a":{"value":32768},"b":{"value":32768}})),
            Some(50.0)
        );
        assert_eq!(volume_percent(&serde_json::json!({})), None);
    }
}
