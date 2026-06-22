from .base import BaseProvider
from .claude import ClaudeProvider
from .codex import CodexProvider
from .copilot import CopilotProvider
from .gemini import GeminiProvider
from .antigravity import AntigravityProvider

# Instantiate all active providers explicitly
ACTIVE_PROVIDERS = [
    ClaudeProvider(),
    CodexProvider(),
    CopilotProvider(),
    GeminiProvider(),
    AntigravityProvider(),
]

__all__ = [
    "BaseProvider",
    "ClaudeProvider",
    "CodexProvider",
    "CopilotProvider",
    "GeminiProvider",
    "AntigravityProvider",
    "ACTIVE_PROVIDERS",
]
