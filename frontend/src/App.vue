<!--
    应用根组件 / Application Root Component

    提供侧边栏导航和主内容区域的整体布局。
    负责：
    - 跟随系统主题自动切换深色/浅色模式
    - 监听 ffmpeg-missing 事件并显示警告
    - 监听 SSE 断开/重连事件，重连后自动刷新页面
    - 监听 startup-warnings 事件，处理不存在的主播和孤立的后处理记录

    Provides the overall layout with sidebar navigation and main content area.
    Responsible for:
    - Auto dark/light mode following system theme
    - Listening for ffmpeg-missing events and showing warnings
    - Listening for SSE disconnect/reconnect events, auto-reloading on reconnect
    - Listening for startup-warnings to handle non-existent streamers and orphaned post-processing records
-->
<script setup lang="ts">
	import { computed, onMounted, onUnmounted, ref } from "vue";
	import { RouterView, useRouter, useRoute } from "vue-router";
	import NotifyLayer from "./components/NotifyLayer.vue";
	import { Button } from "@/components/ui/button";
	import { Users, Video, Workflow, Radio, Search, Settings, Activity } from "@lucide/vue";
	import { call, on, onSseReconnect, onSseDisconnect } from "@/lib/api";
	import { useNotify } from "@/composables/useNotify";
	import { toast as sonnerToast } from "vue-sonner";
	import { useStreamersStore } from "@/stores/streamers";
	import { useI18n } from "vue-i18n";
	import { useScrollbar } from "@/composables/useScrollbar";
	import { loadLocaleFromServer } from "@/i18n";
	import { useModuleLocaleStore } from "@/stores/moduleLocale";

	const router = useRouter();
	const route = useRoute();
	const { toast, confirm } = useNotify();
	const streamersStore = useStreamersStore();
	const { t, locale } = useI18n();
	const moduleLocaleStore = useModuleLocaleStore();

	const mainScrollEl = ref<HTMLElement | null>(null);
	useScrollbar(mainScrollEl);
	const pageTitle = computed(() => navItems.find((item) => item.to === route.path)?.labelKey ?? "nav.streamers");
	const recordingCount = computed(() => streamersStore.streamers.filter((s) => s.is_recording).length);

	/**
	 * 导航项配置 / Navigation items configuration
	 *
	 * 桌面端侧边栏只用文字，移动端底部标签栏使用「图标 + 小号文字」。
	 * Desktop sidebar uses text only; the mobile bottom tab bar uses icon + tiny label.
	 */
	const navItems = [
		{ to: "/", labelKey: "nav.streamers", icon: Users },
		{ to: "/recordings", labelKey: "nav.recordings", icon: Video },
		{ to: "/postprocess", labelKey: "nav.postprocess", icon: Workflow },
		{ to: "/relay", labelKey: "nav.relay", icon: Radio },
		{ to: "/finder", labelKey: "nav.finder", icon: Search },
		{ to: "/settings", labelKey: "nav.settings", icon: Settings },
	];

	/**
	 * 根据参数切换文档根元素的 dark 类，实现深色/浅色主题切换。
	 * Toggle the dark class on the document root element for dark/light theme switching.
	 *
	 * @param dark - 是否应用深色主题 / Whether to apply dark theme
	 */
	function applyTheme(dark: boolean) {
		document.documentElement.classList.toggle("dark", dark);
	}

	// 监听系统主题变化 / Listen for system theme changes
	const mq = window.matchMedia("(prefers-color-scheme: dark)");
	function onThemeChange(e: MediaQueryListEvent) {
		applyTheme(e.matches);
	}

	// 事件取消订阅函数 / Event unsubscribe functions
	let unlistenFfmpeg: (() => void) | null = null;
	let unlistenReconnect: (() => void) | null = null;
	let unlistenDisconnect: (() => void) | null = null;
	let unlistenWarnings: (() => void) | null = null;
	let unlistenLocaleWarnings: (() => void) | null = null;

	/**
	 * 处理启动时的警告事件：
	 * 1. 不存在的主播账号 -> 提示用户并自动删除
	 * 2. 孤立的后处理记录（对应文件已删除）-> 提示用户并清理
	 *
	 * Handle startup warning events:
	 * 1. Non-existent streamer accounts -> prompt user and auto-delete
	 * 2. Orphaned post-processing records (files deleted) -> prompt user and clean up
	 */
	async function handleStartupWarnings(payload: unknown) {
		const w = payload as {
			missing_streamers: string[];
			missing_pp_results: string[];
		};

		if (w.missing_streamers.length > 0) {
			const ok = await confirm({
				title: t("notify.missingStreamers.title"),
				message: t("notify.missingStreamers.message", { list: w.missing_streamers.join("\n") }),
				confirmText: t("notify.missingStreamers.confirm"),
				cancelText: t("notify.missingStreamers.ignore"),
				danger: true,
			});
			if (ok) {
				for (const username of w.missing_streamers) {
					await streamersStore.removeStreamer(username).catch(() => {});
				}
				toast(t("notify.missingStreamers.done", { count: w.missing_streamers.length }), "success");
			}
		}

		if (w.missing_pp_results.length > 0) {
			const ok = await confirm({
				title: t("notify.missingPpResults.title"),
				message: t("notify.missingPpResults.message", { list: w.missing_pp_results.map((p) => p.split(/[\\/]/).pop()).join("\n") }),
				confirmText: t("notify.missingPpResults.confirm"),
				cancelText: t("notify.missingPpResults.ignore"),
			});
			if (ok) {
				await call("remove_missing_pp_results", {
					paths: w.missing_pp_results,
				}).catch(() => {});
				toast(t("notify.missingPpResults.done", { count: w.missing_pp_results.length }), "success");
			}
		}
	}

	onMounted(async () => {
		// 初始化主题并监听系统主题变化 / Initialize theme and listen for system theme changes
		applyTheme(mq.matches);
		mq.addEventListener("change", onThemeChange);

		const { modules: moduleLocales } = await loadLocaleFromServer("en-US");
		locale.value = "en-US";
		moduleLocaleStore.setLocales("en-US", moduleLocales);

		// 监听 ffmpeg 缺失警告 / Listen for ffmpeg missing warning
		unlistenFfmpeg = await on("ffmpeg-missing", (payload) => {
			const p = payload as { message: string };
			toast(p.message, "warning");
		});

		// SSE 重连后倒计时 3 秒刷新页面，确保状态与服务器同步
		// After SSE reconnect, countdown 3 seconds then reload to sync state with server
		unlistenReconnect = onSseReconnect(() => {
			const COUNTDOWN = 3;
			let remaining = COUNTDOWN;
			const id = "reconnect-reload";
			sonnerToast.info(t("notify.reconnected", { n: remaining }), {
				id,
				duration: (COUNTDOWN + 1) * 1000,
			});
			const timer = setInterval(() => {
				remaining--;
				if (remaining > 0) {
					sonnerToast.info(t("notify.reconnected", { n: remaining }), {
						id,
						duration: (remaining + 1) * 1000,
					});
				} else {
					clearInterval(timer);
					window.location.reload();
				}
			}, 1000);
		});

		// 监听 SSE 断开连接 / Listen for SSE disconnect
		unlistenDisconnect = onSseDisconnect(() => {
			toast(t("notify.disconnected"), "warning");
		});

		// 监听启动警告 / Listen for startup warnings
		unlistenWarnings = await on("startup-warnings", handleStartupWarnings);

		// 监听自定义语言文件校验警告 / Listen for custom locale file validation warnings
		unlistenLocaleWarnings = await on(
			"locale-warnings",
			(payload) => {
				const items = payload as Array<{ path: string; reason: string }>;
				for (const item of items) {
					const file = item.path.replace(/\\/g, "/").split("/").pop() ?? item.path;
					toast(`${t("settings.localeFileInvalid", { file })}: ${item.reason}`, "warning");
				}
			},
		);

	});

	onUnmounted(() => {
		// 清理所有事件监听器 / Clean up all event listeners
		mq.removeEventListener("change", onThemeChange);
		unlistenFfmpeg?.();
		unlistenReconnect?.();
		unlistenDisconnect?.();
		unlistenWarnings?.();
		unlistenLocaleWarnings?.();
	});
</script>

<template>
	<!-- 全局布局过渡：setup 页面与主页面之间的切换 / Global layout transition between setup and main -->
	<Transition name="layout" mode="out-in">

		<!-- setup 页面：全屏无侧边栏 / Setup page: full-screen without sidebar -->
		<div v-if="route.path === '/setup'" key="setup" class="contents">
			<RouterView v-slot="{ Component }">
				<Transition name="page" mode="out-in">
					<component :is="Component" :key="route.path" />
				</Transition>
			</RouterView>
			<NotifyLayer />
		</div>

		<!-- 正常布局：侧边栏（≥md）+ 内容区 + 移动端底部标签栏
		     Normal layout: sidebar (≥md) + content + mobile bottom tab bar -->
		<div v-else key="main" class="app-shell-height app-frame grid grid-cols-[68px_minmax(0,1fr)] overflow-hidden">
			<aside class="hidden md:flex border-r border-white/10 bg-[#26231e] flex-col items-center py-5 text-[#b7afa0]">
				<button class="grid size-10 place-items-center rounded-lg bg-[#d88742] text-[#261c12] font-black text-xs" @click="router.push('/')">SR</button>
				<nav class="mt-10 flex flex-col gap-3">
					<button v-for="item in navItems" :key="item.to" type="button" :title="t(item.labelKey)" class="grid size-10 place-items-center rounded-lg transition-colors" :class="route.path === item.to ? 'bg-[#d88742] text-[#261c12]' : 'hover:bg-white/10 hover:text-white'" @click="router.push(item.to)">
						<component :is="item.icon" class="size-[18px]" />
					</button>
				</nav>
				<div class="mt-auto grid size-9 place-items-center rounded-full border border-emerald-400/30 text-emerald-400" title="Recorder online"><Activity class="size-4" /></div>
			</aside>
			<main class="min-w-0 overflow-hidden bg-[#f0eee8] dark:bg-[#191815]">
				<header class="h-[4.5rem] md:h-20 border-b border-[#ded9cc] dark:border-white/8 bg-[#faf9f5] dark:bg-[#211f1b] px-4 md:px-8 flex items-center justify-between gap-4">
					<div class="flex items-center gap-3 min-w-0">
						<div class="md:hidden grid size-9 place-items-center rounded-xl bg-cyan-400 text-slate-950 font-black text-xs">SR</div>
						<div class="min-w-0"><p class="text-[10px] tracking-[0.2em] font-bold uppercase text-[#a16c3e]">Studio / Live operations</p><h1 class="truncate text-base md:text-lg font-bold text-slate-900 dark:text-white">{{ t(pageTitle) }}</h1></div>
					</div>
					<div class="flex items-center gap-2 shrink-0">
						<div class="hidden sm:flex items-center gap-2 rounded-md bg-[#eeece5] dark:bg-white/5 px-3 py-1.5 text-xs font-medium"><span class="size-1.5 rounded-full bg-emerald-500" /> {{ recordingCount }} recording</div>
					</div>
				</header>
				<div ref="mainScrollEl" class="h-[calc(100%-4.5rem)] md:h-[calc(100%-5rem)] overflow-y-auto overflow-x-hidden p-4 md:p-8 pb-24 scrollbar-overlay workspace-surface">
					<RouterView v-slot="{ Component }">
						<Transition name="page" mode="out-in">
							<component :is="Component" :key="route.path" />
						</Transition>
					</RouterView>
				</div>
			</main>

			<!-- 移动端底部标签栏（<md），固定在安全区之上 / Mobile bottom tab bar (<md), above the safe area -->
			<nav
				class="md:hidden fixed inset-x-3 bottom-3 z-40 rounded-xl border border-white/10 bg-[#26231e] shadow-xl"
				:style="{
					paddingBottom: 'env(safe-area-inset-bottom)',
					paddingLeft: 'env(safe-area-inset-left)',
					paddingRight: 'env(safe-area-inset-right)',
				}"
			>
				<div class="flex items-stretch">
					<button
						v-for="item in navItems"
						:key="item.to"
						type="button"
						class="flex flex-1 min-w-0 h-14 flex-col items-center justify-center gap-0.5 px-0 text-[10px] leading-tight tracking-tight transition-colors"
						:class="route.path === item.to ? 'text-[#e9a15b] font-semibold' : 'text-[#b7afa0] active:text-white'"
						:aria-current="route.path === item.to ? 'page' : undefined"
						:aria-label="t(item.labelKey)"
						@click="router.push(item.to)"
					>
						<component :is="item.icon" class="size-5 shrink-0" />
						<span class="w-full truncate text-center">{{
							t(item.labelKey)
						}}</span>
					</button>
				</div>
			</nav>
			<NotifyLayer />
		</div>

	</Transition>
</template>
