// ---------------------------------------------------------------------------
// Re-export everything from bear-core::llm — bear-server uses these directly.
// ---------------------------------------------------------------------------

pub use bear_core::llm::*;

/// Type alias for backward compatibility — all code now uses ChatMessage.
pub type OllamaMessage = ChatMessage;
