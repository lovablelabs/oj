import { useEffect, useRef, useState } from "react";

const GITHUB = "https://github.com/lovablelabs/oj";

const SECTIONS = [
  { id: "how", label: "How it works" },
  { id: "benchmark", label: "Benchmark" },
  { id: "features", label: "Features" },
  { id: "start", label: "Get started" },
] as const;

export function Nav() {
  const [active, setActive] = useState("");
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });

    const obs = new IntersectionObserver(
      (entries) => {
        for (const e of entries) if (e.isIntersecting) setActive(e.target.id);
      },
      { rootMargin: "-45% 0px -50% 0px" },
    );
    for (const s of SECTIONS) {
      const el = document.getElementById(s.id);
      if (el) obs.observe(el);
    }
    return () => {
      window.removeEventListener("scroll", onScroll);
      obs.disconnect();
    };
  }, []);

  return (
    <header className="nav" data-scrolled={scrolled}>
      <div className="wrap nav__inner">
        <a href="#top" className="mark" aria-label="oj home">
          <span className="mark__dot" />
          <span className="mark__word">oj</span>
        </a>
        <nav className="nav__links">
          {SECTIONS.map((s) => (
            <a key={s.id} href={`#${s.id}`} className="nav__link" data-active={active === s.id}>
              {s.label}
            </a>
          ))}
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
          <a className="foot__link" href="#start">
            Get started
          </a>
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

export function Trail() {
  const svgRef = useRef<SVGSVGElement | null>(null);
  const pathRef = useRef<SVGPathElement | null>(null);

  useEffect(() => {
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
    if (window.matchMedia?.("(pointer: coarse)").matches) return;
    const svg = svgRef.current;
    const path = pathRef.current;
    if (!svg || !path) return;

    const segments = 100;
    const points: { x: number; y: number }[] = [];
    const mouse = { x: 0, y: 0 };
    let raf = 0;

    const move = (event: MouseEvent) => {
      mouse.x = event.clientX;
      mouse.y = event.clientY;
      if (points.length === 0) {
        for (let i = 0; i < segments; i++) points.push({ x: mouse.x, y: mouse.y });
      }
    };
    const anim = () => {
      let px = mouse.x;
      let py = mouse.y;
      points.forEach((p, index) => {
        p.x = px;
        p.y = py;
        const n = points[index + 1];
        if (n) {
          px = px - (p.x - n.x) * 0.6;
          py = py - (p.y - n.y) * 0.6;
        }
      });
      if (points.length) {
        path.setAttribute("d", `M ${points.map((p) => `${p.x} ${p.y}`).join(" L ")}`);
      }
      raf = requestAnimationFrame(anim);
    };
    const resize = () => {
      const ww = window.innerWidth;
      const wh = window.innerHeight;
      svg.style.width = ww + "px";
      svg.style.height = wh + "px";
      svg.setAttribute("viewBox", `0 0 ${ww} ${wh}`);
    };

    document.addEventListener("mousemove", move);
    window.addEventListener("resize", resize);
    resize();
    raf = requestAnimationFrame(anim);
    return () => {
      cancelAnimationFrame(raf);
      document.removeEventListener("mousemove", move);
      window.removeEventListener("resize", resize);
    };
  }, []);

  return (
    <svg ref={svgRef} className="trail" viewBox="0 0 1 1" aria-hidden="true">
      <path ref={pathRef} d="" />
    </svg>
  );
}
