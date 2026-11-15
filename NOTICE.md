# Licensing split

## Community Core

Everything in this repository is released under the **GNU Affero General
Public License, version 3** (AGPL-3.0). It contains Community Core only; the
commercial control plane lives in a separate private repository under a
separate agreement.

The plugin SDK/runtime are part of Community Core. AI and storage plugins are
deliberately not gated by a commercial license.

## Why AGPL rather than GPL

It was GPL-3.0 until 2026-11-14. The GPL is triggered by distributing the
program, and a hosted service never distributes it: a modified copy could be
run as a service for other people without its changes ever being published.
Section 13 of the AGPL closes that gap. For anybody running the core for
themselves or for their own customers, nothing changes.

The change was made while every commit in the repository had one author, so
no other contributor's consent was needed. A contribution from now on is
contributed under AGPL-3.0.

## Commercial edition

The commercial control plane is not covered by this license and is not in this
repository. Splitting it out means the boundary is a repository boundary as well
as a service one, so there is no directory here that a reader has to be told to
ignore.
