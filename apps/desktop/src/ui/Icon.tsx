import type { SVGProps } from "react";

const PATHS = {
  cloud: "M17.5 19a4.5 4.5 0 1 0-1.4-8.8A6 6 0 1 0 5 13.5 3.5 3.5 0 0 0 6.5 19h11z",
  check: "M5 12.5l4.5 4.5L19 7.5",
  alert: "M12 3.5l9.5 16.5h-19L12 3.5z M12 10v4.5 M12 17.2v.1",
  play: "M7 4.5v15l12-7.5-12-7.5z",
  refresh: "M20 11a8 8 0 0 0-14.5-4M4 4v4h4M4 13a8 8 0 0 0 14.5 4M20 20v-4h-4",
  folder: "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z",
  sliders: "M4 7h10M18 7h2M4 17h2M10 17h10M14 5v4M6 15v4",
  history: "M12 7v5l3 2M21 12a9 9 0 1 1-3-6.7M21 4v4h-4",
  computer: "M4 5h16v11H4zM2 19h20M9 16v3M15 16v3",
  upload: "M12 16V5M7.5 9.5L12 5l4.5 4.5M5 19h14",
  download: "M12 5v11M7.5 11.5L12 16l4.5-4.5M5 19h14",
  x: "M6 6l12 12M18 6L6 18",
  copy: "M9 9h10v10H9zM5 15V5h10",
  chevron: "M9 5l7 7-7 7",
  plus: "M12 5v14M5 12h14",
  info: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 11v5M12 8v.1",
  activity: "M3 12h4l3-8 4 16 3-8h4",
  link: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1",
  shield: "M12 3l8 3v6c0 4.5-3.2 8-8 9-4.8-1-8-4.5-8-9V6l8-3z",
  trash: "M5 7h14M9 7V4h6v3M7 7l1 13h8l1-13",
  layers: "M12 3l9 5-9 5-9-5 9-5zM3 13l9 5 9-5",
  box: "M12 3l8.5 4.5v9L12 21l-8.5-4.5v-9L12 3zM12 12l8.5-4.5M12 12v9M12 12L3.5 7.5",
  grid: "M4 4h6v6H4zM14 4h6v6h-6zM4 14h6v6H4zM14 14h6v6h-6z",
  star: "M12 3l2.9 5.9 6.5.9-4.7 4.6 1.1 6.5L12 17.8 6.2 20.9l1.1-6.5L2.6 9.8l6.5-.9L12 3z",
  github:
    "M9 19c-4.3 1.4-4.3-2.5-6-3m12 5v-3.5c0-1 .1-1.4-.5-2 2.8-.3 5.5-1.4 5.5-6a4.6 4.6 0 0 0-1.3-3.2 4.2 4.2 0 0 0-.1-3.2s-1.1-.3-3.5 1.3a12.3 12.3 0 0 0-6.2 0C6.5 2.8 5.4 3.1 5.4 3.1a4.2 4.2 0 0 0-.1 3.2A4.6 4.6 0 0 0 4 9.5c0 4.6 2.7 5.7 5.5 6-.6.6-.6 1.2-.5 2V21",
  code: "M16 18l6-6-6-6M8 6l-6 6 6 6",
  sparkles:
    "M12 3l1.6 4.4L18 9l-4.4 1.6L12 15l-1.6-4.4L6 9l4.4-1.6L12 3zM5 17l.8 2.2L8 20l-2.2.8L5 23l-.8-2.2L2 20l2.2-.8L5 17zM19 14l.6 1.6L21.2 16l-1.6.6L19 18.2l-.6-1.6L16.8 16l1.6-.6L19 14z",
  external: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6M15 3h6v6M10 14L21 3",
  key: "M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.78 7.78 5.5 5.5 0 0 1 7.78-7.78zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4",
  user: "M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2M12 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8z",
  calendar: "M8 2v4M16 2v4M3 10h18M5 4h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z",
} as const;

export type IconName = keyof typeof PATHS;

export function Icon({ name, size = 18, ...rest }: { name: IconName; size?: number } & SVGProps<SVGSVGElement>) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
      {...rest}
    >
      <path d={PATHS[name]} />
    </svg>
  );
}
