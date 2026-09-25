use crate::types::io::OutputItem;

/// The type of an output item received during streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SSEItemType {
    MultiAgentCall,
    MultiAgentCallOutput,
    AgentMessage,
    Reasoning,
    FunctionCall,
    ToolSearchCall,
    CustomToolCall,
    WebSearchCall,
    McpCall,
    McpListTools,
    ShellCall,
    Compaction,
    Message,
}

impl SSEItemType {
    pub(crate) fn is_collaboration(self) -> bool {
        matches!(
            self,
            Self::MultiAgentCall | Self::MultiAgentCallOutput | Self::AgentMessage
        )
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MultiAgentCall => "multi_agent_call",
            Self::MultiAgentCallOutput => "multi_agent_call_output",
            Self::AgentMessage => "agent_message",
            Self::Reasoning => "reasoning",
            Self::FunctionCall => "function_call",
            Self::ToolSearchCall => "tool_search_call",
            Self::CustomToolCall => "custom_tool_call",
            Self::WebSearchCall => "web_search_call",
            Self::McpCall => "mcp_call",
            Self::McpListTools => "mcp_list_tools",
            Self::ShellCall => "shell_call",
            Self::Compaction => "compaction",
            Self::Message => "message",
        }
    }
}

impl From<&str> for SSEItemType {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or(Self::Message)
    }
}

impl std::str::FromStr for SSEItemType {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "multi_agent_call" => Ok(Self::MultiAgentCall),
            "multi_agent_call_output" => Ok(Self::MultiAgentCallOutput),
            "agent_message" => Ok(Self::AgentMessage),
            "reasoning" => Ok(Self::Reasoning),
            "function_call" => Ok(Self::FunctionCall),
            "tool_search_call" => Ok(Self::ToolSearchCall),
            "custom_tool_call" => Ok(Self::CustomToolCall),
            "web_search_call" => Ok(Self::WebSearchCall),
            "mcp_call" => Ok(Self::McpCall),
            "mcp_list_tools" => Ok(Self::McpListTools),
            "shell_call" => Ok(Self::ShellCall),
            "compaction" => Ok(Self::Compaction),
            "message" => Ok(Self::Message),
            _ => Err(()),
        }
    }
}

impl TryFrom<&OutputItem> for SSEItemType {
    type Error = ();

    fn try_from(item: &OutputItem) -> Result<Self, Self::Error> {
        match item {
            OutputItem::Message(_) => Ok(Self::Message),
            OutputItem::FunctionCall(_) => Ok(Self::FunctionCall),
            OutputItem::ToolSearchCall(_) => Ok(Self::ToolSearchCall),
            OutputItem::CustomToolCall(_) => Ok(Self::CustomToolCall),
            OutputItem::WebSearchCall(_) => Ok(Self::WebSearchCall),
            OutputItem::McpCall(_) => Ok(Self::McpCall),
            OutputItem::McpListTools(_) => Ok(Self::McpListTools),
            OutputItem::ShellCall(_) => Ok(Self::ShellCall),
            OutputItem::Reasoning(_) => Ok(Self::Reasoning),
            OutputItem::Compaction(_) => Ok(Self::Compaction),
            OutputItem::MultiAgentCall(_) => Ok(Self::MultiAgentCall),
            OutputItem::MultiAgentCallOutput(_) => Ok(Self::MultiAgentCallOutput),
            OutputItem::AgentMessage(_) => Ok(Self::AgentMessage),
            OutputItem::Unknown => Err(()),
        }
    }
}

impl From<String> for SSEItemType {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl PartialEq<str> for SSEItemType {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SSEItemType {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
