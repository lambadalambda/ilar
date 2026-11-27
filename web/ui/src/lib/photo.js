// A photo for a prompt: scaled so its long side is at most `side`
// pixels and re-encoded as JPEG, which turns a 12 MB phone picture into
// a few hundred kB that the wire and the model both take.

const SIDE = 1600;
const QUALITY = 0.85;

/** The scale that fits `width` x `height` in `side`, never above 1. */
export function fit(width, height, side = SIDE) {
  return Math.min(1, side / Math.max(width, height));
}

/** {media_type, data (base64), url (for a thumbnail)} from a File. */
export async function shrink(file) {
  const bitmap = await createImageBitmap(file, { imageOrientation: "from-image" });
  const scale = fit(bitmap.width, bitmap.height);
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(bitmap.width * scale);
  canvas.height = Math.round(bitmap.height * scale);
  canvas.getContext("2d").drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  bitmap.close();
  const blob = await new Promise((resolve, reject) =>
    canvas.toBlob((made) => (made ? resolve(made) : reject(new Error("could not encode the photo"))), "image/jpeg", QUALITY),
  );
  const data = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).split(",")[1]);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
  return { media_type: "image/jpeg", data, url: URL.createObjectURL(blob) };
}
