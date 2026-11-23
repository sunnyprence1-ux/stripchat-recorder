<!--
    主播卡片组件 / Streamer Card Component

    展示单个主播的缩略图、在线状态、观看人数和录制控制按钮。
    通过 useFastThumbnail 对多个 CDN 域名进行竞速，加快缩略图加载速度。

    Displays a single streamer's thumbnail, online status, viewer count, and recording controls.
    Uses useFastThumbnail to race multiple CDN domains for faster thumbnail loading.

    Props:
        streamer - 主播数据对象 / Streamer data object

    Emits:
        remove       - 用户点击移除按钮 / User clicks remove button
        start        - 用户点击开始录制 / User clicks start recording
        stop         - 用户点击停止录制 / User clicks stop recording
        toggle-auto  - 用户切换自动录制开关 / User toggles auto-record switch
-->
<script setup lang="ts">
	import type { StreamerEntry } from "../stores/streamers";
	import { Button } from "@/components/ui/button";
	import { Switch } from "@/components/ui/switch";
	import { Label } from "@/components/ui/label";
	import { ref, watch, computed } from "vue";
	import { useFastThumbnail } from "@/composables/useFastThumbnail";
	import { useI18n } from "vue-i18n";

	const props = defineProps<{ streamer: StreamerEntry }>();
	const detailsOpen = ref(props.streamer.is_recording);
	const emit = defineEmits<{
		remove: [];
		start: [];
		stop: [];
		"toggle-auto": [enabled: boolean];
	}>();
	const { t } = useI18n();

	const autoRecord = ref(props.streamer.auto_record);
	watch(
		() => props.streamer.auto_record,
		(val) => { autoRecord.value = val; },
	);

	watch(() => props.streamer.is_recording, (recording) => { detailsOpen.value = recording; });

	const thumbnailSrc = computed(() => props.streamer.thumbnail_url ?? null);
	const { url: fastThumbnail, retry: retryThumbnail } = useFastThumbnail(thumbnailSrc);

	function onAutoChange(val: boolean) {
		autoRecord.value = val;
		emit("toggle-auto", val);
	}

</script>

<template>
	<article class="signal-row" :class="{ 'signal-row-active': streamer.is_recording }">
		<div class="signal-thumbnail">
			<img v-if="fastThumbnail" :src="fastThumbnail" alt="" loading="lazy" decoding="async" @error="retryThumbnail" />
			<span v-else aria-hidden="true">{{ streamer.username.slice(0, 2).toUpperCase() }}</span>
		</div>
		<div class="signal-identity">
			<h2>{{ streamer.username }}</h2>
			<p>
				<span class="signal-state" :class="{ 'signal-live': streamer.is_online }">{{ streamer.is_online ? streamer.status || 'Live' : t("streamerCard.offline") }}</span>
				<span v-if="streamer.is_recording" class="signal-rec">Recording</span>
				<span v-else-if="streamer.is_online">{{ streamer.viewers.toLocaleString() }} viewers</span>
			</p>
		</div>
		<div class="signal-actions">
			<Button v-if="streamer.is_recording" variant="destructive" @click="emit('stop')">■ Stop</Button>
			<Button v-else :disabled="!streamer.is_recordable" :title="!streamer.is_recordable ? streamer.status : undefined" @click="emit('start')">● Record</Button>
			<Button size="icon" variant="outline" :aria-expanded="detailsOpen" :aria-label="'Options for ' + streamer.username" @click="detailsOpen = !detailsOpen">⋯</Button>
		</div>
		<div v-if="detailsOpen" class="signal-row-details">
			<div class="flex items-center gap-2">
				<Switch :id="'auto-' + streamer.username" :model-value="autoRecord" @update:model-value="onAutoChange" />
				<Label :for="'auto-' + streamer.username">{{ t("streamerCard.autoRecord") }}</Label>
			</div>
			<Button variant="outline" @click="emit('remove')">Remove</Button>
		</div>
	</article>
</template>
