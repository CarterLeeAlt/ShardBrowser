// Lucide 1.77.0, https://lucide.dev. See public/licenses/Lucide.txt.
import type { SVGProps } from "react";

type IconProps = SVGProps<SVGSVGElement> & { size?: number };

function Glyph({ size = 16, ...props }: IconProps) {
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor"
    strokeWidth={2} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false" {...props} />;
}

export const AppWindow = (props: IconProps) => <Glyph {...props}>
  <rect x={"2"} y={"4"} width={"20"} height={"16"} rx={"2"} />
  <path d={"M10 4v4"} />
  <path d={"M2 8h20"} />
  <path d={"M6 4v4"} />
</Glyph>;

export const Building2 = (props: IconProps) => <Glyph {...props}>
  <path d={"M10 12h4"} />
  <path d={"M10 8h4"} />
  <path d={"M14 21v-3a2 2 0 0 0-4 0v3"} />
  <path d={"M6 10H4a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-2"} />
  <path d={"M6 21V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v16"} />
</Glyph>;

export const CircleAlert = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"10"} />
  <line x1={"12"} x2={"12"} y1={"8"} y2={"12"} />
  <line x1={"12"} x2={"12.01"} y1={"16"} y2={"16"} />
</Glyph>;

export const CircleCheck = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"10"} />
  <path d={"m16 9-5.5 5.5L8 12"} />
</Glyph>;

export const Clock = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"10"} />
  <path d={"M12 6v6l4 2"} />
</Glyph>;

export const Copy = (props: IconProps) => <Glyph {...props}>
  <rect width={"14"} height={"14"} x={"8"} y={"8"} rx={"2"} ry={"2"} />
  <path d={"M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"} />
</Glyph>;

export const CopyPlus = (props: IconProps) => <Glyph {...props}>
  <line x1={"15"} x2={"15"} y1={"12"} y2={"18"} />
  <line x1={"12"} x2={"18"} y1={"15"} y2={"15"} />
  <rect width={"14"} height={"14"} x={"8"} y={"8"} rx={"2"} ry={"2"} />
  <path d={"M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"} />
</Glyph>;

export const Download = (props: IconProps) => <Glyph {...props}>
  <path d={"M12 15V3"} />
  <path d={"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"} />
  <path d={"m7 10 5 5 5-5"} />
</Glyph>;

export const EllipsisVertical = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"1"} />
  <circle cx={"12"} cy={"5"} r={"1"} />
  <circle cx={"12"} cy={"19"} r={"1"} />
</Glyph>;

export const FileUp = (props: IconProps) => <Glyph {...props}>
  <path d={"M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z"} />
  <path d={"M14 2v5a1 1 0 0 0 1 1h5"} />
  <path d={"M12 12v6"} />
  <path d={"m15 15-3-3-3 3"} />
</Glyph>;

export const FingerprintPattern = (props: IconProps) => <Glyph {...props}>
  <path d={"M12 10a2 2 0 0 0-2 2c0 1.02-.1 2.51-.26 4"} />
  <path d={"M14 13.12c0 2.38 0 6.38-1 8.88"} />
  <path d={"M17.29 21.02c.12-.6.43-2.3.5-3.02"} />
  <path d={"M2 12a10 10 0 0 1 18-6"} />
  <path d={"M2 16h.01"} />
  <path d={"M21.8 16c.2-2 .131-5.354 0-6"} />
  <path d={"M5 19.5C5.5 18 6 15 6 12a6 6 0 0 1 .34-2"} />
  <path d={"M8.65 22c.21-.66.45-1.32.57-2"} />
  <path d={"M9 6.8a6 6 0 0 1 9 5.2v2"} />
</Glyph>;

export const Folder = (props: IconProps) => <Glyph {...props}>
  <path d={"M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"} />
</Glyph>;

export const FolderOpen = (props: IconProps) => <Glyph {...props}>
  <path d={"m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2"} />
</Glyph>;

export const Globe = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"10"} />
  <path d={"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"} />
  <path d={"M2 12h20"} />
</Glyph>;

export const GripVertical = (props: IconProps) => <Glyph {...props}>
  <circle cx={"9"} cy={"12"} r={"1"} />
  <circle cx={"9"} cy={"5"} r={"1"} />
  <circle cx={"9"} cy={"19"} r={"1"} />
  <circle cx={"15"} cy={"12"} r={"1"} />
  <circle cx={"15"} cy={"5"} r={"1"} />
  <circle cx={"15"} cy={"19"} r={"1"} />
</Glyph>;

export const Info = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"10"} />
  <path d={"M12 16v-4"} />
  <path d={"M12 8h.01"} />
</Glyph>;

export const LoaderCircle = (props: IconProps) => <Glyph {...props}>
  <path d={"M21 12a9 9 0 1 1-6.219-8.56"} />
</Glyph>;

export const Maximize = (props: IconProps) => <Glyph {...props}>
  <path d={"M8 3H5a2 2 0 0 0-2 2v3"} />
  <path d={"M21 8V5a2 2 0 0 0-2-2h-3"} />
  <path d={"M3 16v3a2 2 0 0 0 2 2h3"} />
  <path d={"M16 21h3a2 2 0 0 0 2-2v-3"} />
</Glyph>;

export const Minus = (props: IconProps) => <Glyph {...props}>
  <path d={"M5 12h14"} />
</Glyph>;

export const Moon = (props: IconProps) => <Glyph {...props}>
  <path d={"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"} />
</Glyph>;

export const Network = (props: IconProps) => <Glyph {...props}>
  <rect x={"16"} y={"16"} width={"6"} height={"6"} rx={"1"} />
  <rect x={"2"} y={"16"} width={"6"} height={"6"} rx={"1"} />
  <rect x={"9"} y={"2"} width={"6"} height={"6"} rx={"1"} />
  <path d={"M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3"} />
  <path d={"M12 12V8"} />
</Glyph>;

export const Pencil = (props: IconProps) => <Glyph {...props}>
  <path d={"M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"} />
  <path d={"m15 5 4 4"} />
</Glyph>;

export const Play = (props: IconProps) => <Glyph {...props}>
  <path d={"M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z"} />
</Glyph>;

export const Plus = (props: IconProps) => <Glyph {...props}>
  <path d={"M5 12h14"} />
  <path d={"M12 5v14"} />
</Glyph>;

export const Puzzle = (props: IconProps) => <Glyph {...props}>
  <path d={"M15.39 4.39a1 1 0 0 0 1.68-.474 2.5 2.5 0 1 1 3.014 3.015 1 1 0 0 0-.474 1.68l1.683 1.682a2.414 2.414 0 0 1 0 3.414L19.61 15.39a1 1 0 0 1-1.68-.474 2.5 2.5 0 1 0-3.014 3.015 1 1 0 0 1 .474 1.68l-1.683 1.682a2.414 2.414 0 0 1-3.414 0L8.61 19.61a1 1 0 0 0-1.68.474 2.5 2.5 0 1 1-3.014-3.015 1 1 0 0 0 .474-1.68l-1.683-1.682a2.414 2.414 0 0 1 0-3.414L4.39 8.61a1 1 0 0 1 1.68.474 2.5 2.5 0 1 0 3.014-3.015 1 1 0 0 1-.474-1.68l1.683-1.682a2.414 2.414 0 0 1 3.414 0z"} />
</Glyph>;

export const RefreshCw = (props: IconProps) => <Glyph {...props}>
  <path d={"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"} />
  <path d={"M21 3v5h-5"} />
  <path d={"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"} />
  <path d={"M8 16H3v5"} />
</Glyph>;

export const Save = (props: IconProps) => <Glyph {...props}>
  <path d={"M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"} />
  <path d={"M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7"} />
  <path d={"M7 3v4a1 1 0 0 0 1 1h7"} />
</Glyph>;

export const Search = (props: IconProps) => <Glyph {...props}>
  <path d={"m21 21-4.34-4.34"} />
  <circle cx={"11"} cy={"11"} r={"8"} />
</Glyph>;

export const Settings = (props: IconProps) => <Glyph {...props}>
  <path d={"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"} />
  <circle cx={"12"} cy={"12"} r={"3"} />
</Glyph>;

export const Square = (props: IconProps) => <Glyph {...props}>
  <rect width={"18"} height={"18"} x={"3"} y={"3"} rx={"2"} />
</Glyph>;

export const Sun = (props: IconProps) => <Glyph {...props}>
  <circle cx={"12"} cy={"12"} r={"4"} />
  <path d={"M12 2v2"} />
  <path d={"M12 20v2"} />
  <path d={"m4.93 4.93 1.41 1.41"} />
  <path d={"m17.66 17.66 1.41 1.41"} />
  <path d={"M2 12h2"} />
  <path d={"M20 12h2"} />
  <path d={"m6.34 17.66-1.41 1.41"} />
  <path d={"m19.07 4.93-1.41 1.41"} />
</Glyph>;

export const Trash = (props: IconProps) => <Glyph {...props}>
  <path d={"M10 11v6"} />
  <path d={"M14 11v6"} />
  <path d={"M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"} />
  <path d={"M3 6h18"} />
  <path d={"M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"} />
</Glyph>;

export const Upload = (props: IconProps) => <Glyph {...props}>
  <path d={"M12 3v12"} />
  <path d={"m17 8-5-5-5 5"} />
  <path d={"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"} />
</Glyph>;

export const X = (props: IconProps) => <Glyph {...props}>
  <path d={"M18 6 6 18"} />
  <path d={"m6 6 12 12"} />
</Glyph>;
