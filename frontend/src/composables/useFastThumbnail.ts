/**
 * Lazy thumbnail source selection with sequential CDN fallback.
 * The browser starts the first request only when the image enters the viewport;
 * alternate hosts are tried only after the current source fails.
 */
import { ref, watch, type Ref } from "vue";

const CDN_TLDS = [
	"doppiocdn.com",
	"doppiocdn.org",
	"doppiocdn.live",
	"doppiocdn.net",
];

export function useFastThumbnail(thumbnailUrl: Ref<string | null | undefined>) {
	const resolvedUrl = ref<string | null>(null);
	let candidates: string[] = [];
	let candidateIndex = 0;

	function retry() {
		const nextIndex = candidateIndex + 1;
		if (nextIndex >= candidates.length) {
			resolvedUrl.value = null;
			return;
		}
		candidateIndex = nextIndex;
		resolvedUrl.value = candidates[candidateIndex];
	}

	watch(
		thumbnailUrl,
		(url) => {
			candidateIndex = 0;
			if (!url) {
				candidates = [];
				resolvedUrl.value = null;
				return;
			}

			const matchedTld = CDN_TLDS.find((tld) => url.includes(tld));
			candidates = matchedTld
				? [matchedTld, ...CDN_TLDS.filter((tld) => tld !== matchedTld)].map((tld) => url.replace(matchedTld, tld))
				: [url];
			resolvedUrl.value = candidates[0];
		},
		{ immediate: true },
	);

	return { url: resolvedUrl, retry };
}
