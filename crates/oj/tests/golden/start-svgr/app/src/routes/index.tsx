import { createFileRoute } from "@tanstack/react-router";

import robots from "/robots.txt?url";
import Star from "../icons/star.svg?react";
import starUrl from "../icons/star.svg";
import dots from "../icons/dots.svg?url";

export const Route = createFileRoute("/")({
  component: () => (
    <main>
      <Star width={24} height={24} aria-hidden />
      <img src={starUrl} alt="star" />
      <img src={dots} alt="" />
      <a href={robots}>robots</a>
      <a href={new URL("../icons/dots.svg", import.meta.url).href}>dots</a>
    </main>
  ),
});
