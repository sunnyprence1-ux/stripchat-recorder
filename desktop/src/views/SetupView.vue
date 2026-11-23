<!--
    首次启动配置向导 / First-launch Setup Wizard

    在 setup_done = false 时显示，引导用户完成基础配置：
    Step 1 — 语言选择
    Step 2 — 录制输出目录
    Step 3 — 网络代理（可选）
    完成后将 setup_done 置为 true 并跳转到主页。

    Shown when setup_done = false. Guides the user through basic configuration:
    Step 1 — Language
    Step 2 — Recording output directory
    Step 3 — Network proxies (optional)
    On finish, sets setup_done = true and navigates to the home page.
-->
<script setup lang="ts">
import { ref, computed, onMounted } from "vue";
import { useRouter } from "vue-router";
import { useI18n } from "vue-i18n";
import { useSettingsStore } from "@/stores/settings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

const router = useRouter();
const { t } = useI18n();
const store = useSettingsStore();

// ── 步骤控制 / Step control ──────────────────────────────────────────────────
const TOTAL_STEPS = 2;
const step = ref(1);
const saving = ref(false);
const error = ref("");

/** 步骤切换方向，用于决定滑动动画方向 / Step direction for slide animation */
const stepTransition = ref("step-forward");

// ── 表单字段 / Form fields ───────────────────────────────────────────────────
const outputDir = ref("");
const apiProxy = ref("");
const scMirror = ref("");
const cdnProxy = ref("");

/** 可用语言列表（从共享 store 读取，由 App.vue 统一维护）
 * Available locales (from shared store, maintained by App.vue) */
// 直接使用 store 的 ref，保持响应式；不能赋给普通变量（会丢失响应式）
// Use the store's ref directly to keep reactivity; assigning to a plain variable breaks it

onMounted(async () => {
	// App.vue 可能还未完成 onMounted（例如首次配置路由直接到 /setup），
	// 确保语言列表已加载；若已加载则 refresh() 内部会快速返回已有结果。
	// App.vue may not have finished its onMounted yet (e.g. first-launch routing to /setup).
	// Ensure the locale list is loaded; if already loaded, refresh() returns quickly.
	await store.fetchSettings();
	const s = store.settings;
	outputDir.value = s.output_dir || "";
	apiProxy.value = s.api_proxy_url || "";
	scMirror.value = s.sc_mirror_url || "";
	cdnProxy.value = s.cdn_proxy_url || "";
});

// ── 步骤校验 / Step validation ───────────────────────────────────────────────
const canNext = computed(() => {
	if (step.value === 1) return outputDir.value.trim().length > 0;
	return true;
});

function next() {
	error.value = "";
	if (step.value < TOTAL_STEPS) {
		stepTransition.value = "step-forward";
		step.value++;
	}
}

function back() {
	error.value = "";
	if (step.value > 1) {
		stepTransition.value = "step-back";
		step.value--;
	}
}

// ── 完成向导 / Finish wizard ─────────────────────────────────────────────────
async function finish() {
	if (!outputDir.value.trim()) {
		error.value = t("setup.outputDirRequired");
		stepTransition.value = "step-back";
		step.value = 1;
		return;
	}
	saving.value = true;
	error.value = "";
	try {
		await store.saveSettings({
			...store.settings,
			language: "en-US",
			output_dir: outputDir.value.trim(),
			api_proxy_url: apiProxy.value.trim() || null,
			sc_mirror_url: scMirror.value.trim() || null,
			cdn_proxy_url: cdnProxy.value.trim() || null,
			setup_done: true,
		});
		await router.replace("/");
	} catch (e: unknown) {
		error.value = String(e);
	} finally {
		saving.value = false;
	}
}
</script>

<template>
	<div class="min-h-screen flex items-center justify-center bg-[#26231e] p-4 md:p-8">
		<div class="w-full max-w-3xl grid md:grid-cols-[0.8fr_1.2fr] overflow-hidden rounded-2xl border border-white/10 bg-card shadow-2xl">
			<aside class="hidden md:flex flex-col justify-between bg-[#302c25] p-8 text-[#eee7d8]"><div><span class="grid size-11 place-items-center rounded-lg bg-[#d88742] text-[#261c12] font-black text-xs">SR</span><p class="mt-8 text-xs font-bold uppercase tracking-[0.2em] text-[#e6a364]">Capture studio</p><h2 class="mt-3 text-3xl font-bold">Set up your recording space.</h2></div><p class="text-xs text-white/50">Stripchat Recorder · English edition</p></aside>
			<div class="w-full p-5 md:p-9 flex flex-col gap-6 md:gap-8">

			<!-- 标题 / Title -->
			<div class="flex flex-col gap-1.5">
				<div class="flex items-center gap-2.5">
					<span class="w-3 h-3 rounded-full bg-destructive shrink-0" />
					<span class="text-lg font-bold">StripchatRecorder</span>
				</div>
				<h1 class="text-2xl font-bold mt-1">{{ t("setup.title") }}</h1>
				<p class="text-sm text-muted-foreground">{{ t("setup.subtitle") }}</p>
			</div>

			<!-- 步骤指示器 / Step indicator -->
			<div class="flex items-center gap-2">
				<template v-for="i in TOTAL_STEPS" :key="i">
					<div
						class="flex items-center justify-center w-7 h-7 rounded-full text-xs font-semibold transition-colors duration-200"
						:class="
							i < step
								? 'bg-primary text-primary-foreground'
								: i === step
									? 'bg-primary text-primary-foreground ring-2 ring-primary/30'
									: 'bg-muted text-muted-foreground'
						"
					>
						<svg v-if="i < step" class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3">
							<polyline points="20 6 9 17 4 12" />
						</svg>
						<span v-else>{{ i }}</span>
					</div>
					<div
						v-if="i < TOTAL_STEPS"
						class="flex-1 h-px transition-colors duration-200"
						:class="i < step ? 'bg-primary' : 'bg-border'"
					/>
				</template>
			</div>

			<!-- 步骤内容（带方向感知的滑动过渡）/ Step content with directional slide transition -->
			<div class="overflow-hidden min-h-52">
				<Transition :name="stepTransition" mode="out-in">
					<div :key="step" class="flex flex-col gap-6">

						<!-- Step 1: output directory -->
						<template v-if="step === 1">
							<div class="flex flex-col gap-1.5">
								<h2 class="text-base font-semibold">{{ t("setup.step2.title") }}</h2>
								<p class="text-sm text-muted-foreground">{{ t("setup.step2.desc") }}</p>
							</div>
							<div class="flex flex-col gap-2">
								<Label>{{ t("settings.outputDir.label") }}</Label>
								<Input
									v-model="outputDir"
									:placeholder="t('setup.step2.placeholder')"
									autocomplete="off"
									autofocus
								/>
								<p class="text-xs text-muted-foreground">{{ t("setup.step2.hint") }}</p>
							</div>
						</template>

						<!-- Step 2: Network proxies (optional) -->
						<template v-if="step === 2">
							<div class="flex flex-col gap-1.5">
								<h2 class="text-base font-semibold">{{ t("setup.step3.title") }}</h2>
								<p class="text-sm text-muted-foreground">{{ t("setup.step3.desc") }}</p>
							</div>
							<div class="flex flex-col gap-4">
								<div class="flex flex-col gap-1.5">
									<Label>{{ t("settings.apiProxy.label") }}</Label>
									<Input v-model="apiProxy" :placeholder="t('settings.apiProxy.placeholder')" autocomplete="off" />
								</div>
								<div class="flex flex-col gap-1.5">
									<Label>{{ t("settings.scMirror.label") }}</Label>
									<Input v-model="scMirror" :placeholder="t('settings.scMirror.placeholder')" autocomplete="off" />
								</div>
								<div class="flex flex-col gap-1.5">
									<Label>{{ t("settings.cdnProxy.label") }}</Label>
									<Input v-model="cdnProxy" :placeholder="t('settings.cdnProxy.placeholder')" autocomplete="off" />
								</div>
							</div>
						</template>

					</div>
				</Transition>
			</div>

			<!-- 错误提示 / Error message -->
			<p v-if="error" class="text-sm text-destructive -mt-2">{{ error }}</p>

			<!-- 导航按钮 / Navigation buttons -->
			<div class="flex items-center justify-between">
				<Button variant="ghost" :disabled="step === 1" @click="back">
					{{ t("setup.back") }}
				</Button>

				<div class="flex items-center gap-3">
					<Button v-if="step < TOTAL_STEPS" :disabled="!canNext" @click="next">
						{{ t("setup.next") }}
					</Button>
					<Button v-if="step === TOTAL_STEPS" :disabled="saving" @click="finish">
						{{ saving ? t("setup.saving") : t("setup.finish") }}
					</Button>
				</div>
			</div>

			</div>
		</div>
	</div>
</template>
