<!--
    主播列表页面 / Streamer List View

    展示所有被追踪主播的卡片网格，支持添加/移除主播、手动开始/停止录制、切换自动录制。
    页面挂载时初始化事件监听器并从后端加载主播列表。

    Displays a card grid of all tracked streamers, supporting add/remove streamers,
    manual start/stop recording, and toggling auto-record.
    Initializes event listeners and loads the streamer list from backend on mount.
-->
<script setup lang="ts">
	import { computed, onMounted, ref } from "vue";
	import { Radio, CircleDotDashed, UsersRound } from "@lucide/vue";
	import { useStreamersStore } from "../stores/streamers";
	import type { StreamerEntry } from "../stores/streamers";
	import { useNotify } from "../composables/useNotify";
	import { useMergingStore } from "../stores/merging";
	import { usePpStatusStore } from "../stores/ppStatus";
	import StreamerCard from "../components/StreamerCard.vue";
	import AddStreamerDialog from "../components/AddStreamerDialog.vue";
	import { Button } from "@/components/ui/button";
	import { useI18n } from "vue-i18n";

	const store = useStreamersStore();
	const mergingStore = useMergingStore();
	const ppStatusStore = usePpStatusStore();
	const { toast, confirm } = useNotify();
	const { t } = useI18n();
	const startAllTargets = computed(() => store.streamers.filter((s) => s.is_recordable && !s.is_recording));
	const onlineCount = computed(() => store.streamers.filter((s) => s.is_online).length);
	const recordingCount = computed(() => store.streamers.filter((s) => s.is_recording).length);
	const totalViewers = computed(() => store.streamers.reduce((total, s) => total + (s.is_online ? s.viewers : 0), 0));
	const sortedStreamers = computed(() => [...store.streamers].sort((a, b) => a.username.localeCompare(b.username)));
	/** 是否显示添加主播对话框 / Whether to show the add streamer dialog */
	const showAdd = ref(false);

	onMounted(async () => {
		store.initListeners();
		await store.fetchStreamers();
	});

	/**
	 * 处理移除主播操作，先弹出确认对话框。
	 * 删除前取消该主播所有正在进行的后处理任务，并清理合并队列状态。
	 *
	 * Handle remove streamer action with confirmation dialog.
	 * Cancels all in-progress post-processing tasks and clears merge queue state before removal.
	 *
	 * @param username - 主播用户名 / Streamer username
	 */
	async function handleRemove(username: string) {
		const ok = await confirm({
			title: t("home.remove.title"),
			message: t("home.remove.message", { username }),
			confirmText: t("home.remove.confirm"),
			danger: true,
		});
		if (!ok) return;
		try {
			// 取消并清理该主播的后处理任务 / Cancel and clear post-processing tasks for this streamer
			await ppStatusStore.cancelAndClearForUsername(username);
			// 清理该主播的合并队列状态 / Clear merge queue state for this streamer
			mergingStore.clearMergingForUsername(username);
			await store.removeStreamer(username);
			toast(t("home.remove.done", { username }), "success");
		} catch (e) {
			toast(String(e), "error");
		}
	}

	/**
	 * 处理手动开始录制操作。
	 * Handle manual start recording action.
	 *
	 * @param username - 主播用户名 / Streamer username
	 */
	async function handleStart(username: string) {
		try {
			await store.startRecording(username);
			toast(t("home.start.done", { username }), "success");
		} catch (e) {
			toast(String(e), "error");
		}
	}

	/**
	 * 处理自动录制开关切换。
	 * 若开启自动录制且主播当前可录制但未在录制，则立即开始录制。
	 *
	 * Handle auto-record toggle.
	 * If enabled and streamer is currently recordable but not recording, start recording immediately.
	 *
	 * @param username - 主播用户名 / Streamer username
	 * @param streamer - 主播数据对象 / Streamer data object
	 * @param enabled - 是否开启自动录制 / Whether to enable auto-record
	 */
	async function handleToggleAuto(
		username: string,
		streamer: StreamerEntry,
		enabled: boolean,
	) {
		try {
			await store.setAutoRecord(username, enabled);
			if (enabled && streamer.is_recordable && !streamer.is_recording) {
				await store.startRecording(username);
				toast(t("home.start.autoStarted", { username }), "success");
			}
		} catch (e) {
			toast(String(e), "error");
		}
	}

	/**
	 * 处理停止录制操作，先弹出确认对话框。
	 * Handle stop recording action with confirmation dialog.
	 *
	 * @param username - 主播用户名 / Streamer username
	 */
	async function handleStop(username: string) {
		const ok = await confirm({
			title: t("home.stop.title"),
			message: t("home.stop.message", { username }),
			confirmText: t("home.stop.confirm"),
			danger: true,
		});
		if (!ok) return;
		try {
			await store.stopRecording(username);
			toast(t("home.stop.done", { username }), "info");
		} catch (e) {
			toast(String(e), "error");
		}
	}
</script>

<template>
	<div class="flex flex-col gap-7">
		<section class="rounded-2xl bg-[#302c25] p-5 md:p-7 text-white">
			<div class="flex flex-wrap items-end justify-between gap-5"><div><p class="text-xs uppercase tracking-[0.18em] font-bold text-[#e6a364]">Recording desk</p><h2 class="mt-2 text-2xl md:text-3xl font-bold tracking-tight">Your stream watchlist</h2><p class="mt-2 max-w-lg text-sm text-white/65">Monitor live rooms, start a capture, and keep recordings moving.</p></div><Button class="rounded-lg bg-[#d88742] text-[#261c12] hover:bg-[#e6a364]" @click="showAdd = true">{{ t("home.addStreamer") }}</Button></div>
		</section>
		<section class="grid grid-cols-2 lg:grid-cols-3 gap-3">
			<div class="rounded-xl border border-border bg-card p-4"><div class="flex items-center justify-between text-muted-foreground"><span class="text-xs font-bold uppercase tracking-wider">Watching</span><UsersRound class="size-4 text-primary" /></div><p class="mt-3 text-2xl font-bold">{{ store.streamers.length }}</p><p class="text-xs text-muted-foreground">tracked rooms</p></div>
			<div class="rounded-xl border border-border bg-card p-4"><div class="flex items-center justify-between text-muted-foreground"><span class="text-xs font-bold uppercase tracking-wider">Live now</span><Radio class="size-4 text-emerald-500" /></div><p class="mt-3 text-2xl font-bold">{{ onlineCount }}</p><p class="text-xs text-muted-foreground">{{ totalViewers.toLocaleString() }} viewers</p></div>
			<div class="col-span-2 lg:col-span-1 rounded-xl border border-border bg-card p-4"><div class="flex items-center justify-between text-muted-foreground"><span class="text-xs font-bold uppercase tracking-wider">Capturing</span><CircleDotDashed class="size-4 text-rose-500" /></div><p class="mt-3 text-2xl font-bold">{{ recordingCount }}</p><p class="text-xs text-muted-foreground">active recordings</p></div>
		</section>
		<header class="flex flex-wrap items-end justify-between gap-3 border-b border-border pb-4"><div><h2 class="text-lg font-bold">Streamers</h2><p class="text-sm text-muted-foreground">{{ t("home.subtitle", { total: store.streamers.length, recording: recordingCount }) }}</p></div></header>

		<div
			v-if="store.loading && store.streamers.length === 0"
			class="text-center text-muted-foreground py-16"
		>
			{{ t("home.loadingStreamers") }}
		</div>

		<div
			v-else-if="store.streamers.length === 0"
			class="text-center text-muted-foreground py-16 flex flex-col items-center gap-3"
		>
			<p>{{ t("home.noStreamers") }}</p>
			<Button @click="showAdd = true">{{ t("home.addFirst") }}</Button>
		</div>

		<div
			v-else
			class="grid grid-cols-[repeat(auto-fill,minmax(min(100%,280px),1fr))] gap-4"
		>
			<StreamerCard
				v-for="s in sortedStreamers"
				:key="s.username"
				:streamer="s"
				@remove="handleRemove(s.username)"
				@toggle-auto="handleToggleAuto(s.username, s, $event)"
				@start="handleStart(s.username)"
				@stop="handleStop(s.username)"
			/>
		</div>

		<AddStreamerDialog
			v-if="showAdd"
			@close="showAdd = false"
			@added="showAdd = false"
		/>
	</div>
</template>

