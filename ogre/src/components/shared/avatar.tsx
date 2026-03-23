interface AvatarProps {
  src: string;
  size?: number;
}

export function Avatar({ src, size = 64 }: AvatarProps) {
  const avatarSrc = src.includes("avatar.tangled.sh")
    ? src.replace(/[?&]format=\w+/, "").replace(/[?&]$/, "") +
      (src.includes("?") ? "&" : "?") + "format=jpeg"
    : src;

  return (
    <div
      style={{
        width: size,
        height: size,
        borderRadius: size / 2,
        overflow: "hidden",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
      }}>
      <img
        src={avatarSrc}
        width={size}
        height={size}
        style={{ objectFit: "cover" }}
      />
    </div>
  );
}
