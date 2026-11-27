import "./settings.css";

export default function Settings({ items }: { items: string[] }) {
  return (
    <ul className="settings">
      {items.map((item) => (
        <li key={item}>{item}</li>
      ))}
    </ul>
  );
}
