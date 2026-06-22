from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any


class BaseProvider(ABC):
    """Abstract base class for all usage and quota providers."""

    @property
    @abstractmethod
    def key(self) -> str:
        """The short unique key of the provider (e.g., 'claude', 'gemini')."""
        pass

    @property
    @abstractmethod
    def name(self) -> str:
        """The human-readable display name of the provider."""
        pass

    @property
    @abstractmethod
    def cache_ttl_seconds(self) -> int:
        """The default caching TTL in seconds for the provider."""
        pass

    @abstractmethod
    def retrieve_quota(self) -> dict[str, Any]:
        """Retrieve the live usage/quota or return an error result."""
        pass
