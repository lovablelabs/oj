import { useEffect, useId, useState } from "react";

const GITHUB = "https://github.com/lovablelabs/oj";

// The Lovable heart mark, ported from the pulse design system's
// LovableLogoColor: four radial-gradient layers clipped to the heart path,
// stops fitted to the Figma export. Kept verbatim so the mark renders
// identically to lovable.dev.
interface HeartMeshLayer {
  fill: string;
  cx: number;
  cy: number;
  rx: number;
  ry: number;
  stops: [offset: number, opacity: number][];
}

const HEART_PATH =
  "M6.89785 0C10.7074 0 13.7957 3.17898 13.7957 7.10046V9.79908H16.0913C19.9009 9.79908 22.9892 12.9781 22.9892 16.8995C22.9892 20.821 19.9009 24 16.0913 24H0V7.10046C0 3.17898 3.08827 0 6.89785 0Z";

const HEART_LAYERS: HeartMeshLayer[] = [
  {
    fill: "#4B73FF",
    cx: 10.084,
    cy: 12.811,
    rx: 27.376,
    ry: 27.791,
    stops: [
      [0, 1], [0.1625, 0.9984], [0.2417, 0.9911], [0.3042, 0.9716], [0.3333, 0.9543],
      [0.3583, 0.9331], [0.3833, 0.9052], [0.4083, 0.8697], [0.4375, 0.8177], [0.4625, 0.7633],
      [0.4917, 0.6902], [0.5042, 0.6547], [0.5417, 0.5458], [0.6, 0.3687], [0.6208, 0.3143],
      [0.6292, 0.2888], [0.6375, 0.2708], [0.6542, 0.226], [0.6583, 0.2219], [0.6667, 0.1993],
      [0.675, 0.1859], [0.6792, 0.173], [0.6833, 0.1695], [0.6917, 0.1495], [0.7083, 0.1234],
      [0.7333, 0.0892], [0.7625, 0.0582], [0.7958, 0.0339], [0.8208, 0.0219], [0.8917, 0.0053],
      [1, 0],
    ],
  },
  {
    fill: "#FF66F4",
    cx: 11.794,
    cy: 4.043,
    rx: 31.745,
    ry: 27.791,
    stops: [
      [0, 1], [0.1958, 0.999], [0.275, 0.9941], [0.3333, 0.9809], [0.3833, 0.9538],
      [0.4, 0.9355], [0.4208, 0.9023], [0.4583, 0.8277], [0.4833, 0.7688], [0.4958, 0.7373],
      [0.5042, 0.7105], [0.5167, 0.6781], [0.6125, 0.3744], [0.65, 0.2677], [0.6875, 0.1798],
      [0.7083, 0.1395], [0.725, 0.1132], [0.75, 0.0798], [0.7667, 0.0631], [0.8, 0.0372],
      [0.825, 0.0239], [0.8542, 0.0137], [0.9, 0.0054], [1, 0],
    ],
  },
  {
    fill: "#FF0105",
    cx: 15.045,
    cy: 1.037,
    rx: 27.376,
    ry: 25.845,
    stops: [
      [0, 0.9998], [0.1375, 0.9979], [0.2125, 0.9908], [0.25, 0.9818], [0.2792, 0.9704],
      [0.325, 0.9403], [0.3625, 0.8983], [0.3792, 0.8752], [0.4, 0.8412], [0.4333, 0.7749],
      [0.4583, 0.7175], [0.4958, 0.6192], [0.6125, 0.2858], [0.6458, 0.2082], [0.6667, 0.1668],
      [0.6833, 0.1375], [0.7083, 0.1015], [0.7333, 0.0732], [0.7625, 0.0478], [0.7875, 0.0325],
      [0.8208, 0.0184], [0.8917, 0.0047], [1, 0],
    ],
  },
  {
    fill: "#FE7B02",
    cx: 12.071,
    cy: 4.039,
    rx: 21.173,
    ry: 21.423,
    stops: [
      [0, 0.9705], [0.0667, 0.9615], [0.1, 0.9503], [0.1292, 0.9358], [0.1833, 0.8954],
      [0.2208, 0.8554], [0.2417, 0.8281], [0.2875, 0.7565], [0.3417, 0.6529], [0.3875, 0.5537],
      [0.4667, 0.3745], [0.5208, 0.2634], [0.55, 0.2122], [0.5833, 0.161], [0.6167, 0.1184],
      [0.6417, 0.0923], [0.6708, 0.0673], [0.7083, 0.0433], [0.775, 0.0177], [0.8583, 0.0047],
      [1, 0],
    ],
  },
];

export function LovableHeart({ className }: { className?: string }) {
  const id = useId();
  const clipId = `${id}-heart`;
  return (
    <svg viewBox="0 0 23 24" fill="none" xmlns="http://www.w3.org/2000/svg" className={className} aria-hidden="true">
      <defs>
        <clipPath id={clipId} clipPathUnits="userSpaceOnUse">
          <path clipRule="evenodd" d={HEART_PATH} />
        </clipPath>
        {HEART_LAYERS.map((layer, index) => (
          <radialGradient
            key={index}
            id={`${id}-${index}`}
            cx="0"
            cy="0"
            r="1"
            gradientUnits="userSpaceOnUse"
            gradientTransform={`translate(${layer.cx} ${layer.cy}) scale(${layer.rx} ${layer.ry})`}
          >
            {layer.stops.map(([offset, opacity]) => (
              <stop key={offset} offset={offset} stopColor={layer.fill} stopOpacity={opacity} />
            ))}
          </radialGradient>
        ))}
      </defs>
      <g clipPath={`url(#${clipId})`}>
        {HEART_LAYERS.map((_, index) => (
          <rect key={index} x="0" y="0" width={23} height={24} fill={`url(#${id}-${index})`} />
        ))}
      </g>
    </svg>
  );
}

export function Nav() {
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  return (
    <header className="nav" data-scrolled={scrolled}>
      <div className="wrap nav__inner">
        <a href="#top" className="mark" aria-label="oj home">
          <LovableHeart className="mark__heart" />
          <span className="mark__word">oj</span>
          <span className="badge">wasm</span>
        </a>
        <nav className="nav__links">
          <a className="nav__link" href={GITHUB} target="_blank" rel="noreferrer">
            GitHub
          </a>
        </nav>
      </div>
    </header>
  );
}

export function Footer() {
  return (
    <footer className="foot">
      <div className="wrap foot__row">
        <span className="foot__note">
          oj is an open-source project created by{" "}
          <a href="https://github.com/raphamorim" target="_blank" rel="noreferrer">
            Raphael Amorim
          </a>{" "}
          and maintained by{" "}
          <a href="https://lovable.dev" target="_blank" rel="noreferrer">
            Lovable
          </a>
          . This site is built with oj and deployed on Cloudflare.
        </span>
        <div className="foot__links">
          <a className="foot__link" href={GITHUB} target="_blank" rel="noreferrer">
            GitHub
          </a>
          <a className="foot__link" href={`${GITHUB}/issues`} target="_blank" rel="noreferrer">
            Issues
          </a>
        </div>
      </div>
    </footer>
  );
}
