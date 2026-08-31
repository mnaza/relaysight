# Licensing split

## Community Core

Everything in this repository is currently intended to be released under **GPL-3.0** for the open-source self-hosted edition. It contains Community Core only; the commercial control plane lives in a separate private repository under a separate agreement.

The plugin SDK/runtime are part of Community Core. AI and storage plugins are deliberately not gated by a commercial license.

## Commercial edition

The commercial control plane is not covered by this license and is not in this repository. Splitting it out means the boundary is a repository boundary as well as a service one, so there is no directory here that a reader has to be told to ignore.

Before public launch, have counsel confirm the final license split. If protection against third parties offering modified hosted versions without publishing changes is important, evaluate AGPL-3.0 for Community Core before accepting outside contributions.
