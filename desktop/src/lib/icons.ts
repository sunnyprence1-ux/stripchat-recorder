import { defineComponent, h } from "vue";

// Signal Room uses compact 24px symbols with square terminals.
function icon(name: string, path: string, filled = false) {
	return defineComponent({
		name,
		setup: () => () => h("svg", {
			width: 24, height: 24, viewBox: "0 0 24 24", "aria-hidden": "true",
			fill: filled ? "currentColor" : "none",
			stroke: filled ? "none" : "currentColor", "stroke-width": 2,
		}, [h("path", { d: path })]),
	});
}

export const Activity = icon("Activity", "M3 16h3l3-8 4 11 3-7h5");
export const Users = icon("Users", "M4 4h7v7H4zM14 4h6v7h-6zM2 20v-4l2-2h7l2 2v4zM15 14h5l2 2v4h-6");
export const Video = icon("Video", "M3 4h18v16H3zM3 8h18M10 11l5 3-5 3z");
export const Workflow = icon("Workflow", "M3 3h6v6H3zM15 15h6v6h-6zM6 9v9h9M12 6h9v5");
export const Radio = icon("Radio", "M4 8l4-4h8l4 4v8l-4 4H8l-4-4zM9 9h6v6H9z");
export const Search = icon("Search", "M4 4h11v11H4zM15 15l5 5");
export const Settings = icon("Settings", "M3 6h7m4 0h7M3 12h13m4 0h1M3 18h2m4 0h12M10 3h4v6h-4zM16 9h4v6h-4zM5 15h4v6H5z");
export const Check = icon("Check", "m3 12 6 6L21 6");
export const Copy = icon("Copy", "M8 8h12v12H8zM4 16V4h12");
export const Wifi = icon("Wifi", "M2 8l4-4h12l4 4M6 12l3-3h6l3 3M9 16l3-3 3 3m-3 3v2");
export const WifiOff = icon("WifiOff", "M2 8l4-4h12l4 4M8 11l2-2h5l3 3m-6 7v2M3 3l18 18");
export const AlertCircle = icon("AlertCircle", "M7 3h10l4 4v10l-4 4H7l-4-4V7zM12 7v6m0 3v2");
export const Loader = icon("Loader", "M21 12v5l-4 4H7l-4-4V7l4-4h5");
export const Loader2 = Loader;
export const Square = icon("Square", "M5 5h14v14H5z", true);
export const Record = icon("Record", "M8 4h8l4 4v8l-4 4H8l-4-4V8z", true);
export const Brand = icon("Brand", "M3 18h4v-5H3zm7 0h4V9h-4zm7 0h4V5h-4z", true);
export const More = icon("More", "M4 4h6v6H4zm10 0h6v6h-6zM4 14h6v6H4zm10 0h6v6h-6z", true);
export const Send = icon("Send", "m3 4 18 8-18 8 4-8zM7 12h14");
export const GripVertical = icon("GripVertical", "M7 5h10M7 12h10M7 19h10");
export const ChevronUp = icon("ChevronUp", "m5 15 7-7 7 7M8 18h8");
export const ChevronDown = icon("ChevronDown", "m5 9 7 7 7-7M8 6h8");
export const X = icon("X", "M6 6l12 12M18 6 6 18");
export const FlaskConical = icon("FlaskConical", "M8 3h8m-6 0v7l-6 8 2 3h12l2-3-6-8V3M8 15h8");
export const KeyRound = icon("KeyRound", "M3 4h9v9H3zM8 13v7h7v-3h4v-4h-7M6 8h3");
export const RefreshCw = icon("RefreshCw", "M20 4v6h-6M4 20v-6h6M4 10l4-5h8l4 5M20 14l-4 5H8l-4-5");
export const ImageIcon = icon("ImageIcon", "M3 3h18v18H3zM4 17l5-6 4 5 3-3 4 4M15 7h3");
export const ArrowUp = icon("ArrowUp", "M12 21V4m-6 6 6-6 6 6");
export const ArrowDown = icon("ArrowDown", "M12 3v17m-6-6 6 6 6-6");
export const ArrowUpDown = icon("ArrowUpDown", "M7 19V5m-4 4 4-4 4 4m6-4v14m-4-4 4 4 4-4");
export const Plus = icon("Plus", "M11 4h2v7h7v2h-7v7h-2v-7H4v-2h7z", true);
export const Minus = icon("Minus", "M4 11h16v2H4z", true);
export const CircleIcon = Record;
export const CircleCheckIcon = icon("CircleCheckIcon", "M7 3h10l4 4v10l-4 4H7l-4-4V7zM7 12l3 3 7-7");
export const InfoIcon = icon("InfoIcon", "M3 3h18v18H3zM12 6v2m-2 3h2v6m-2 0h5");
export const Loader2Icon = Loader;
export const OctagonXIcon = icon("OctagonXIcon", "M7 3h10l4 4v10l-4 4H7l-4-4V7zM8 8l8 8m0-8-8 8");
export const TriangleAlertIcon = icon("TriangleAlertIcon", "m12 3 10 17H2zM12 9v5m0 2v2");
export const TriangleAlert = TriangleAlertIcon;
export const XIcon = X;
export const Trash2 = icon("Trash2", "M3 6h18M9 6V3h6v3M6 6l1 15h10l1-15M10 10v7m4-7v7");
