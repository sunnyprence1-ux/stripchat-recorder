<!--
    添加主播对话框组件 / Add Streamer Dialog Component

    提供一个模态对话框，允许用户通过输入 Stripchat 用户名来添加新主播到追踪列表。
    提交时调用 streamersStore.addStreamer 并通过 toast 反馈操作结果。

    Provides a modal dialog for users to add a new streamer to the tracking list
    by entering their Stripchat username. Calls streamersStore.addStreamer on submit
    and provides toast feedback for the operation result.

    Emits:
        close - 对话框关闭时触发 / Emitted when dialog is closed
        added - 主播成功添加后触发 / Emitted after streamer is successfully added
-->
<script setup lang="ts">
	import { computed, onMounted, onUnmounted, ref, watch } from "vue";
	import { useStreamersStore } from "../stores/streamers";
	import { useNotify } from "../composables/useNotify";
	import {
		Dialog,
		DialogContent,
		DialogDescription,
		DialogHeader,
		DialogTitle,
		DialogFooter,
	} from "@/components/ui/dialog";
	import { Button } from "@/components/ui/button";
	import { Input } from "@/components/ui/input";
	import { Label } from "@/components/ui/label";
	import { useI18n } from "vue-i18n";

	const emit = defineEmits<{ close: []; added: [] }>();
	const store = useStreamersStore();
	const { toast } = useNotify();
	const { t } = useI18n();
	const username = ref("");
	const loading = ref(false);

	type LiveRoom = {
		username: string;
		thumbnail_url: string | null;
		thumbnail_urls?: string[];
		country?: string | null;
		viewers?: number | null;
	};
	const tab = ref<"browse" | "manual">("browse");
	const rooms = ref<LiveRoom[]>([]);
	const nextOffset = ref<number | null>(0);
	const browsing = ref(false);
	const browseError = ref("");
	const addingRoom = ref<string | null>(null);
	const imageIndex = ref<Record<string, number>>({});
	const search = ref("");
	const country = ref("");
	const hideAdded = ref(false);
	const regionNames = typeof Intl.DisplayNames === "function"
		? new Intl.DisplayNames(["en"], { type: "region" })
		: null;

	function countryName(value: string) {
		return /^[a-z]{2}$/i.test(value) ? regionNames?.of(value.toUpperCase()) ?? value : value;
	}

	function thumbnail(room: LiveRoom) {
		const choices = room.thumbnail_urls ?? (room.thumbnail_url ? [room.thumbnail_url] : []);
		return choices[imageIndex.value[room.username] ?? 0];
	}
	const tracked = computed(() => new Set(store.streamers.map((entry) => entry.username.toLowerCase())));
	// ISO 3166-1 alpha-2 codes keep the picker usable before those countries appear in the directory.
	const countryCodes = "AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ BA BB BD BE BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ DE DJ DK DM DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR GA GB GD GE GF GG GH GI GL GM GN GP GQ GR GS GT GU GW GY HK HM HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP KE KG KH KI KM KN KP KR KW KY KZ LA LB LC LI LK LR LS LT LU LV LY MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ NA NC NE NF NG NI NL NO NP NR NU NZ OM PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW SA SB SC SD SE SG SH SI SJ SK SL SM SN SO SR SS ST SV SX SY SZ TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ VA VC VE VG VI VN VU WF WS YE YT ZA ZM ZW".split(" ");
	const countryNameToCode = new Map(countryCodes.map((code) => [countryName(code).toLowerCase(), code]));
	function countryKey(value?: string | null) {
		const name = value?.trim() ?? "";
		if (/^[a-z]{2}$/i.test(name)) return name.toUpperCase() === "UK" ? "GB" : name.toUpperCase();
		return countryNameToCode.get(name.toLowerCase()) ?? name;
	}
	const countries = computed(() => [...new Set([
		...countryCodes,
		...rooms.value.map((room) => countryKey(room.country)).filter(Boolean),
	])].sort((a, b) => countryName(a).localeCompare(countryName(b))));

	const filteredRooms = computed(() => {
		const query = search.value.trim().toLowerCase();
		return rooms.value.filter((room) =>
			(!query || room.username.toLowerCase().includes(query)) &&
			(!country.value || countryKey(room.country) === country.value) &&
			(!hideAdded.value || !tracked.value.has(room.username.toLowerCase()))
		);
	});

	const controller = new AbortController();
	const pageSignatures = new Set<string>();

	async function loadRooms() {
		const offset = nextOffset.value;
		if (browsing.value || offset === null) return;
		browsing.value = true;
		browseError.value = "";
		try {
			const response = await fetch(`/api/live-rooms?offset=${offset}`, {
				signal: controller.signal,
			});
			const contentType = response.headers.get("content-type") ?? "";
			if (response.status === 404 || contentType.includes("text/html")) {
				browseError.value = "The live room API is unavailable. Update the recorder backend and check that your reverse proxy forwards /api/live-rooms.";
				return;
			}
			if (!response.ok) {
				const detail = response.status === 400 && contentType.includes("text/plain")
					? await response.text()
					: "";
				browseError.value = detail || `The recorder returned HTTP ${response.status}. Try again later.`;
				return;
			}
			const page = await response.json() as { rooms: LiveRoom[]; next_offset: number | null };
			if (!Array.isArray(page.rooms)) {
				browseError.value = "The recorder returned an unexpected live room response.";
				return;
			}
			const signature = page.rooms.map((room) => room.username.toLowerCase()).sort().join("|");
			if (signature && pageSignatures.has(signature)) {
				browseError.value = "The directory repeated a page. Retry to continue searching.";
				return;
			}
			if (page.next_offset !== null && (!Number.isSafeInteger(page.next_offset) || page.next_offset <= offset)) {
				browseError.value = "The directory did not provide a valid next page.";
				return;
			}
			if (signature) pageSignatures.add(signature);
			const seen = new Set(rooms.value.map((room) => room.username.toLowerCase()));
			for (const room of page.rooms) {
				const name = room.username.toLowerCase();
				if (seen.has(name)) continue;
				seen.add(name);
				rooms.value.push(room);
			}
			nextOffset.value = page.next_offset;
		} catch {
			if (!controller.signal.aborted) browseError.value = "Could not load live rooms. Try again or add a room manually.";
		} finally {
			browsing.value = false;
		}
	}

	async function addRoom(room: LiveRoom) {
		const name = room.username.toLowerCase();
		if (addingRoom.value || tracked.value.has(name)) return;
		addingRoom.value = name;
		try {
			await store.addStreamer(name);
			toast(t("addStreamer.done", { name }), "success");
		} catch (error) {
			toast(String(error), "error");
		} finally {
			addingRoom.value = null;
		}
	}

	// Continue as each page finishes while a country is selected.
	watch([country, tab, browsing], () => {
		if (tab.value === "browse" && country.value && !browseError.value && !controller.signal.aborted) {
			void loadRooms();
		}
	});

	onMounted(loadRooms);
	onUnmounted(() => controller.abort());

	/**
	 * 从输入中提取用户名：支持直接输入用户名、stripchat.com 链接或 mirror 链接。
	 * Extract username from input: supports plain username, stripchat.com URL, or mirror URL.
	 */
	function extractUsername(input: string): string {
		const trimmed = input.trim();
		try {
			const url = new URL(trimmed.startsWith("http") ? trimmed : `https://${trimmed}`);
			// 匹配 stripchat.com 或任意 mirror 域名下的 /<username> 路径
			const parts = url.pathname.split("/").filter(Boolean);
			if (parts.length > 0) return parts[0];
		} catch {
			// 不是 URL，直接当用户名
		}
		return trimmed;
	}

	/**
	 * 提交表单：验证输入、调用 addStreamer、反馈结果。
	 * Submit the form: validate input, call addStreamer, provide feedback.
	 */
	async function submit() {
		const name = extractUsername(username.value);
		if (!name) return;
		loading.value = true;
		try {
			await store.addStreamer(name);
			toast(t("addStreamer.done", { name }), "success");
			emit("added");
		} catch (e) {
			toast(String(e), "error");
		} finally {
			loading.value = false;
		}
	}
</script>

<template>
	<Dialog :open="true" @update:open="(v) => !v && emit('close')">
		<DialogContent class="sm:max-w-2xl dialog-max-h overflow-y-auto">
			<DialogHeader>
				<DialogTitle>{{ t("addStreamer.title") }}</DialogTitle>
				<DialogDescription class="sr-only">{{ t("addStreamer.description") }}</DialogDescription>
			</DialogHeader>

			<div class="flex flex-wrap gap-2" role="group" aria-label="Add method">
				<Button type="button" :variant="tab === 'browse' ? 'default' : 'outline'" :aria-pressed="tab === 'browse'" @click="tab = 'browse'">Browse live rooms</Button>
				<Button type="button" :variant="tab === 'manual' ? 'default' : 'outline'" :aria-pressed="tab === 'manual'" :disabled="!!addingRoom" @click="tab = 'manual'">Add manually</Button>
			</div>

			<section v-if="tab === 'browse'" aria-label="Live rooms" class="grid gap-3">
				<div class="grid gap-2 sm:grid-cols-2">
					<label class="grid gap-1 text-sm">Username
						<Input v-model="search" type="search" placeholder="Filter by username" />
					</label>
					<label class="grid gap-1 text-sm">Country
						<select v-model="country" class="min-h-10 rounded-md border bg-card px-2">
							<option value="">All countries</option>
							<option v-for="value in countries" :key="value" :value="value">{{ countryName(value) }}</option>
						</select>
					</label>
					<label class="flex min-h-10 items-center gap-2 text-sm sm:col-span-2">
						<input v-model="hideAdded" type="checkbox" /> Hide already added rooms
					</label>
					<p class="text-xs text-muted-foreground sm:col-span-2" role="status">{{ country ? `${filteredRooms.length} matching rooms${browsing ? " · Searching directory…" : ""}` : `${filteredRooms.length} of ${rooms.length} loaded rooms` }}</p>
				</div>
				<div v-if="filteredRooms.length" class="grid grid-cols-2 gap-2 sm:grid-cols-3">
					<article v-for="room in filteredRooms" :key="room.username" class="min-w-0 overflow-hidden rounded-md border bg-card">
						<div class="relative grid aspect-[3/4] place-items-center overflow-hidden bg-muted">
							<img v-if="thumbnail(room)" :key="thumbnail(room)" :src="thumbnail(room)"
								alt="" loading="lazy" decoding="async" referrerpolicy="no-referrer"
								class="absolute inset-0 size-full object-contain" @error="imageIndex[room.username] = (imageIndex[room.username] ?? 0) + 1" />
							<span v-else aria-hidden="true" class="font-mono text-muted-foreground">{{ room.username.slice(0, 2).toUpperCase() }}</span>
						</div>
						<div class="grid gap-2 p-2">
							<h3 class="break-all text-sm font-semibold">{{ room.username }}</h3>
							<p v-if="room.country || room.viewers != null" class="text-xs text-muted-foreground">
								<span v-if="room.country">{{ countryName(room.country) }}</span>
								<span v-if="room.country && room.viewers != null"> · </span>
								<span v-if="room.viewers != null">{{ room.viewers.toLocaleString() }} viewers</span>
							</p>
							<Button size="sm" :disabled="!!addingRoom || tracked.has(room.username.toLowerCase())"
								:aria-label="'Add ' + room.username" @click="addRoom(room)">
								{{ tracked.has(room.username.toLowerCase()) ? 'Added' : addingRoom === room.username.toLowerCase() ? 'Adding…' : 'Add' }}
							</Button>
						</div>
					</article>
				</div>
				<p v-if="browseError" role="alert" class="text-sm text-muted-foreground">{{ browseError }}</p>
				<p v-else-if="!browsing && !filteredRooms.length" role="status" class="text-sm text-muted-foreground">
					{{ rooms.length ? "No rooms match these filters." : "No live rooms on this page." }}
				</p>
				<Button v-if="nextOffset !== null && (!country || browseError)" type="button" variant="outline" :disabled="browsing" @click="loadRooms">
					{{ browsing ? 'Loading live rooms…' : browseError ? 'Retry' : 'Load more' }}
				</Button>
			</section>

			<form v-if="tab === 'manual'" @submit.prevent="submit" class="flex flex-col gap-4 py-2">
				<div class="flex flex-col gap-2">
					<Label for="username">{{ t("addStreamer.label") }}</Label>
				<Input
						id="username"
						v-model="username"
						:placeholder="t('addStreamer.placeholder')"
						:disabled="loading"
					/>
				</div>

				<DialogFooter>
					<Button type="button" variant="outline" @click="emit('close')">
						{{ t("addStreamer.cancel") }}
					</Button>
					<Button type="submit" :disabled="loading || !username.trim()">
						{{ loading ? t("addStreamer.submitting") : t("addStreamer.submit") }}
					</Button>
				</DialogFooter>
			</form>
		</DialogContent>
	</Dialog>
</template>

