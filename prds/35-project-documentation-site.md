# PRD #35: Project Documentation Site

**Status**: Draft
**Priority**: Low
**Created**: 2026-04-03
**GitHub Issue**: [#35](https://github.com/vfarcic/dot-agent-deck/issues/35)

## Problem Statement

The project lacks a dedicated site for documentation, guides, and feature overview. Everything currently lives in the README, which becomes harder to navigate as the project grows. New users have no polished entry point to understand what dot-agent-deck does or how to get started.

## Solution Overview

Create a documentation site generated from a `docs/` directory in this repo and deployed to a Kubernetes cluster. The site should cover installation, usage guides, configuration, and feature overview.

## Scope

### In Scope
- `docs/` directory in this repo as the content source
- Static site generation (tool TBD — e.g., mdBook, Hugo, Astro)
- Deployment to a Kubernetes cluster
- Core content: installation, usage, configuration, keybindings, feature overview

### Out of Scope
- Blog / news section (future enhancement)
- User accounts or interactive features
- Custom domain setup (can be added later)

## Technical Approach

- **Content source**: `docs/` directory in this repository
- **Site generator**: TBD — evaluate options during implementation
- **Hosting**: Kubernetes cluster
- **CI/CD**: Automated build and deploy on changes to `docs/`

## Success Criteria

- Site is accessible and serves documentation from the `docs/` directory
- Content covers the core user journey (install, configure, use)
- Site rebuilds and redeploys automatically when docs change
- Runs on a Kubernetes cluster

## Milestones

- [ ] `docs/` directory created with initial content structure
- [ ] Static site generator chosen and configured
- [ ] Core documentation pages written (install, usage, configuration, keybindings)
- [ ] Kubernetes deployment manifests created
- [ ] CI/CD pipeline for automated build and deploy on docs changes
- [ ] Site live and accessible

## Key Files

- `docs/` — Documentation content source
- TBD — Site generator config, Kubernetes manifests, CI pipeline

## Risks

- **Generator choice**: Picking the wrong tool could require migration later. Mitigate by evaluating a few options before committing.
- **Content maintenance**: Docs can drift from code. Mitigate by including doc updates in feature PRD milestones.
