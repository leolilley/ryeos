<script lang="ts">
  import type { RyeOsChromeVm, RyeOsSessionVm, RyeOsTransportVm } from "../generated";

  interface Props {
    chrome: RyeOsChromeVm;
    session: RyeOsSessionVm;
    transport: RyeOsTransportVm;
  }

  let { chrome, session, transport }: Props = $props();
</script>

<header class="system-bar">
  <div class="brand" aria-label="RyeOS">
    <span class="brand-mark" aria-hidden="true">◇</span>
    <span>{chrome.title}</span>
  </div>
  <div class="system-context">
    <span class="presence" data-tone={chrome.health_tone}>●</span>
    <span>{session.user_principal_id ?? "local"}</span>
    <span class="separator">/</span>
    <span>{session.project_path ?? session.surface_ref}</span>
  </div>
  <div class="system-state">
    <span>{chrome.health_label}</span>
    <span class="transport" data-freshness={transport.freshness}>{transport.freshness}</span>
  </div>
</header>
