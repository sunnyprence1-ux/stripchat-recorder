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
	/** 是否显示添加主播对话框 / Whether to show the add streamer dialog */
	const showAdd = ref(false);
	/** 是否正在批量开始录制 / Whether bulk start is in progress */
	const bulkStarting = ref(false);
	/** 是否正在批量停止录制 / Whether bulk stop is in progress */
	const bulkStopping = ref(false);

	/** 可批量开始录制的主播（在线且可录制、当前未录制）/ Streamers eligible for bulk start */
	const startAllTargets = computed(() =>
		store.streamers.filter((s) => s.is_recordable && !s.is_recording),
	);
	/** 当前正在录制的主播 / Streamers eligible for bulk stop */
	const stopAllTargets = computed(() =>
		store.streamers.filter((s) => s.is_recording),
	);
	const onlineCount = computed(() => store.streamers.filter((s) => s.is_online).length);
	const sortedStreamers = computed(() => [...store.streamers].sort((a, b) => a.username.localeCompare(b.username)));

	const filters = ["All", "Live", "Recording", "Offline"] as const;
	const filter = ref<(typeof filters)[number]>("All");
	const visibleStreamers = computed(() => sortedStreamers.value.filter((entry) => {
		switch (filter.value) {
			case "Live": return entry.is_online;
			case "Recording": return entry.is_recording;
			case "Offline": return !entry.is_online;
			default: return true;
		}
	}));

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

	/**
	 * 一键开始所有可录制主播。逐个调用现有开始接口，避免低配 VPS 上同时发起过多请求。
	 * Start every recordable streamer sequentially to avoid bursts on the low-resource VPS.
	 */
	async function handleStartAll() {
		const targets = [...startAllTargets.value];
		if (targets.length === 0) {
			toast(t("home.bulkStart.none"), "info");
			return;
		}

		bulkStarting.value = true;
		let succeeded = 0;
		const failed: string[] = [];
		try {
			for (const s of targets) {
				try {
					await store.startRecording(s.username);
					succeeded++;
				} catch {
					failed.push(s.username);
				}
			}
			await store.fetchStreamers();
			toast(
				t("home.bulkStart.done", {
					success: succeeded,
					total: targets.length,
				}),
				failed.length === 0 ? "success" : "warning",
			);
			if (failed.length > 0) {
				toast(
					t("home.bulkStart.failed", {
						count: failed.length,
						usernames: failed.join(", "),
					}),
					"error",
				);
			}
		} finally {
			bulkStarting.value = false;
		}
	}

	/**
	 * 一键停止所有正在录制的主播。逐个调用现有停止接口，并先确认危险操作。
	 * Stop every recording sequentially after confirmation, reusing the existing stop path.
	 */
	async function handleStopAll() {
		const targets = [...stopAllTargets.value];
		if (targets.length === 0) {
			toast(t("home.bulkStop.none"), "info");
			return;
		}

		const ok = await confirm({
			title: t("home.bulkStop.title"),
			message: t("home.bulkStop.message", { count: targets.length }),
			confirmText: t("home.bulkStop.confirm"),
			danger: true,
		});
		if (!ok) return;

		bulkStopping.value = true;
		let succeeded = 0;
		const failed: string[] = [];
		try {
			for (const s of targets) {
				try {
					await store.stopRecording(s.username);
					succeeded++;
				} catch {
					failed.push(s.username);
				}
			}
			await store.fetchStreamers();
			toast(
				t("home.bulkStop.done", {
					success: succeeded,
					total: targets.length,
				}),
				failed.length === 0 ? "success" : "warning",
			);
			if (failed.length > 0) {
				toast(
					t("home.bulkStop.failed", {
						count: failed.length,
						usernames: failed.join(", "),
					}),
					"error",
				);
			}
		} finally {
			bulkStopping.value = false;
		}
	}
</script>

<template>
	<section class="signal-watchlist">
		<header class="signal-watch-header">
			<h1>Watchlist</h1>
			<Button @click="showAdd = true">+ {{ t("home.addStreamer") }}</Button>
		</header>
		<dl class="signal-metrics">
			<div><dt>Tracked</dt><dd>{{ store.streamers.length }}</dd></div>
			<div><dt>Live</dt><dd>{{ onlineCount }}</dd></div>
			<div><dt>Recording</dt><dd>{{ stopAllTargets.length }}</dd></div>
		</dl>
		<div class="signal-toolbar">
			<div class="signal-filters" role="group" aria-label="Filter watchlist">
				<button v-for="option in filters" :key="option" type="button"
					:aria-pressed="filter === option" @click="filter = option">{{ option }}</button>
			</div>
			<div v-if="store.streamers.length > 0" class="flex flex-wrap items-center gap-2">
				<Button
					variant="outline"
					size="sm"
					:disabled="startAllTargets.length === 0 || bulkStarting || bulkStopping"
					@click="handleStartAll"
				>
					{{ t("home.startAll", { count: startAllTargets.length }) }}
				</Button>
				<Button
					variant="destructive"
					size="sm"
					:disabled="stopAllTargets.length === 0 || bulkStarting || bulkStopping"
					@click="handleStopAll"
				>
					{{ t("home.stopAll", { count: stopAllTargets.length }) }}
				</Button>
			</div>
		</div>
		<p v-if="store.loading && !store.streamers.length" class="signal-empty" role="status">{{ t("home.loadingStreamers") }}</p>
		<p v-else-if="!store.streamers.length" class="signal-empty">{{ t("home.noStreamers") }}</p>
		<p v-else-if="!visibleStreamers.length" class="signal-empty" role="status">No streams match this filter.</p>
		<div v-else class="signal-rundown">
			<StreamerCard v-for="s in visibleStreamers" :key="s.username" :streamer="s"
				@remove="handleRemove(s.username)"
				@toggle-auto="handleToggleAuto(s.username, s, $event)"
				@start="handleStart(s.username)"
				@stop="handleStop(s.username)" />
		</div>

		<AddStreamerDialog
			v-if="showAdd"
			@close="showAdd = false"
			@added="showAdd = false"
		/>
	</section>
</template>
