export default function Image({ src, alt = "", fill, priority, unoptimized, loader, quality, placeholder, ...props }) {
  return <img
    {...props}
    src={typeof src === "string" ? src : src?.src}
    alt={alt}
    loading={priority ? "eager" : (props.loading ?? "lazy")}
    style={fill ? { position: "absolute", width: "100%", height: "100%", inset: 0, ...props.style } : props.style}
  />;
}
