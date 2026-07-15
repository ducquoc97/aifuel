# AI Fuel

AI Fuel identifies locally configured AI coding providers and reports their subscription quota.

## Language

**Supported Provider**:
An AI coding provider for which AI Fuel has a built-in quota integration. Supported providers form a static catalog and are not necessarily present on a user's machine.
_Avoid_: Active provider, available provider

**Provider Discovery**:
A fast, local, side-effect-free check of which providers have a provider-specific credential source present. Discovery runs before each quota collection and never contacts provider APIs, refreshes tokens, or writes credentials.
_Avoid_: Provider validation, provider authentication

**Discovered Provider**:
An AI coding provider whose provider-specific local credential source is present. A discovered provider remains visible when credential validation or quota retrieval fails.
_Avoid_: Found provider, installed provider, authenticated provider

**Discovery Failure**:
The condition where AI Fuel cannot determine whether a provider-specific credential source is present. The provider is excluded from the discovered set, while the application reports the failure separately.
_Avoid_: Undiscovered provider, provider error

**Provider Credential Source**:
A local credential store created specifically for an AI coding provider. A general account login, such as GitHub CLI authentication, is not a credential source for a related provider such as GitHub Copilot.
_Avoid_: Shared login, reusable credential
