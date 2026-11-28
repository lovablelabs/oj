import "./chart.css";

export default function Chart({ points }: { points: number[] }) {
  return (
    <svg className="chart">
      {points.map((p, i) => (
        <rect key={i} x={i * 10} height={p} width={8} />
      ))}
    </svg>
  );
}
