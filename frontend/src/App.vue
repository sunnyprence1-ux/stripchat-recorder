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
	import { Brand, More, Users, Video, Workflow, Radio, Search, Settings, Activity } from "@/lib/icons";
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

	const primaryNav = navItems.slice(0, 2);
	const moreNav = navItems.slice(2);
	const moreOpen = ref(false);

	function navigateMobile(to: string) {
		moreOpen.value = false;
		void router.push(to);
	}

	function navLabel(item: (typeof navItems)[number]) {
		if (item.to === "/") return "Watchlist";
		if (item.to === "/recordings") return "Library";
		return t(item.labelKey);
	}


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
		<div v-else key="main" class="app-shell-height app-frame signal-shell grid grid-cols-1 md:grid-cols-[68px_minmax(0,1fr)] lg:grid-cols-[172px_minmax(0,1fr)] overflow-hidden">
			<aside class="hidden md:flex border-r border-white/10 bg-[#101b15] flex-col items-center lg:items-stretch py-5 text-[#a8b8ae]">
				<button class="flex items-center gap-3 px-3 lg:px-5" aria-label="Go to watchlist" @click="router.push('/')">
					<span class="grid size-10 shrink-0 place-items-center rounded-xl bg-[#c2f368] text-[#142007] font-black text-xs"><Brand class="size-5" /></span>
					<span class="hidden lg:block text-left"><span class="block text-sm font-bold tracking-wide text-[#eef2e9]">Signal Room</span><span class="mt-0.5 block text-[9px] uppercase tracking-[0.18em] text-[#a79d8a]">Stream operations</span></span>
				</button>
				<p class="hidden lg:block px-5 mt-9 mb-2 text-[9px] uppercase tracking-[0.2em] font-bold text-white/35">Workspace</p>
				<nav class="mt-9 lg:mt-0 flex flex-col gap-1.5 px-2 lg:px-3">
					<button v-for="item in navItems" :key="item.to" type="button" :title="navLabel(item)" :aria-current="route.path === item.to ? 'page' : undefined" class="flex h-11 w-full items-center justify-center lg:justify-start gap-3 rounded-lg px-2 lg:px-3 text-sm transition-colors" :class="route.path === item.to ? 'bg-[#c2f368] text-[#142007] font-semibold' : 'hover:bg-white/8 hover:text-white'" @click="router.push(item.to)">
						<component :is="item.icon" class="size-[18px] shrink-0" />
						<span class="hidden lg:inline truncate">{{ navLabel(item) }}</span>
					</button>
				</nav>
				<div class="mt-auto border-t border-white/10 pt-4 px-2 lg:px-4">
					<div class="flex items-center justify-center lg:justify-start gap-2 rounded-lg bg-white/5 px-2 lg:px-3 py-2.5 text-xs text-[#a8b8ae]" :title="`${recordingCount} active recordings`"><Activity class="size-4 shrink-0" :class="recordingCount > 0 ? 'text-rose-400' : 'text-[#a79d8a]'" /><span class="hidden lg:inline">{{ recordingCount }} active {{ recordingCount === 1 ? 'recording' : 'recordings' }}</span></div>
				</div>
			</aside>
			<main class="min-w-0 overflow-hidden bg-[#0e1712] dark:bg-[#0e1712]">
				<header class="h-[4.5rem] md:h-[4.75rem] border-b border-[#34493d] dark:border-white/8 bg-[#121f18] dark:bg-[#121f18] px-4 md:px-8 flex items-center justify-between gap-4">
					<div class="flex items-center gap-3 min-w-0">
						<div class="md:hidden grid size-9 place-items-center rounded-xl bg-[#c2f368] text-[#142007] font-black text-xs"><Brand class="size-5" /></div>
						<div class="min-w-0"><p class="text-[9px] md:text-[10px] tracking-[0.18em] font-bold uppercase text-[#bbd3c3]">Signal Room</p><p v-if="route.path !== '/'" class="truncate text-base md:text-lg font-bold text-foreground dark:text-white">{{ t(pageTitle) }}</p></div>
					</div>
					<div class="flex items-center gap-2 shrink-0">
						<div class="flex shrink-0 items-center gap-2 rounded-full border border-border bg-[#1d3025] dark:bg-white/5 px-3 py-1.5 text-xs font-medium"><span class="size-1.5 rounded-full" :class="recordingCount > 0 ? 'bg-rose-500' : 'bg-[#a79d8a]'" /> {{ recordingCount }} {{ recordingCount === 1 ? 'recording' : 'recordings' }}</div>
					</div>
				</header>
				<div ref="mainScrollEl" class="h-[calc(100%-4.5rem)] md:h-[calc(100%-4.75rem)] overflow-y-auto overflow-x-hidden p-3 sm:p-4 md:p-7 xl:p-9 pb-[calc(6rem_+_env(safe-area-inset-bottom))] md:pb-8 scrollbar-overlay workspace-surface">
					<RouterView v-slot="{ Component }">
						<Transition name="page" mode="out-in">
							<component :is="Component" :key="route.path" />
						</Transition>
					</RouterView>
				</div>
			</main>

			<!-- 移动端底部标签栏（<md），固定在安全区之上 / Mobile bottom tab bar (<md), above the safe area -->
			<nav class="signal-dock md:hidden" aria-label="Main navigation" @keydown.esc="moreOpen = false">
				<button v-for="item in primaryNav" :key="item.to" type="button"
					:aria-current="route.path === item.to ? 'page' : undefined" @click="navigateMobile(item.to)">
					<component :is="item.icon" class="size-5" />
					{{ navLabel(item) }}
				</button>
				<button type="button" :aria-expanded="moreOpen" aria-controls="signal-more-nav"
					:class="{ 'signal-dock-active': moreNav.some((item) => item.to === route.path) }"
					@click="moreOpen = !moreOpen">
					<More class="size-5" /> More
				</button>
				<div v-if="moreOpen" id="signal-more-nav" class="signal-more-nav">
					<button v-for="item in moreNav" :key="item.to" type="button"
						:aria-current="route.path === item.to ? 'page' : undefined" @click="navigateMobile(item.to)">
						<component :is="item.icon" class="size-5" />
						{{ navLabel(item) }}
					</button>
				</div>
			</nav>
			<NotifyLayer />
		</div>

	</Transition>
</template>
