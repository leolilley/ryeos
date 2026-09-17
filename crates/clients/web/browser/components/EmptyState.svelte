<script lang="ts">interface Props { title: string; message: string } let { title, message }: Props = $props();</script>
<div class="empty-state"><span>◇</span><strong>{title}</strong><p>{message}</p></div>
