from .base import BaseProvider
from .claude import ClaudeProvider
from .codex import CodexProvider
from .copilot import CopilotProvider
from .gemini import GeminiProvider
from .antigravity import AntigravityProvider

# Explicit catalog of integrations that AI Fuel supports.
SUPPORTED_PROVIDER_CLASSES = [
    ClaudeProvider,
    CodexProvider,
    CopilotProvider,
    GeminiProvider,
    AntigravityProvider,
]

__all__ = [
    "BaseProvider",
    "ClaudeProvider",
    "CodexProvider",
    "CopilotProvider",
    "GeminiProvider",
    "AntigravityProvider",
    "SUPPORTED_PROVIDER_CLASSES",
]
