// Test-only independent JBIG2 sample oracle. PDFium's WASM decoder is supplied
// by the caller's pdfjs-dist installation; it is not a production dependency.
import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const [modulePath, wasmDirectory, sourcePath, samplePath, metadataPath] = process.argv.slice(2);
if (!metadataPath) throw new Error('Expected pdfjs module, wasm directory, PDF, sample output, metadata output');
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const { getDocument, OPS, version } = await import(pathToFileURL(path.resolve(modulePath)).href);
const source = readFileSync(sourcePath);
const document = await getDocument({
  data: new Uint8Array(source), wasmUrl: path.resolve(wasmDirectory) + path.sep,
  useWasm: true, isOffscreenCanvasSupported: false, isImageDecoderSupported: false,
}).promise;
if (document.numPages !== 1) throw new Error('This focused oracle requires a single-page fixture');
const page = await document.getPage(1);
const operators = await page.getOperatorList();
const images = [];
for (let index = 0; index < operators.fnArray.length; index++) {
  if (operators.fnArray[index] === OPS.paintImageXObject) {
    images.push(await new Promise(resolve => page.objs.get(operators.argsArray[index][0], resolve)));
  }
}
if (images.length !== 1 || images[0].kind !== 1) {
  throw new Error('Expected exactly one packed one-bit image');
}
const image = images[0], samples = Buffer.from(image.data), stride = Math.ceil(image.width / 8);
if (samples.length !== stride * image.height) throw new Error('Incorrect decoded sample length');
// Padding outside the image width has no pixel semantics.
if (image.width % 8) {
  const mask = (255 << (8 - image.width % 8)) & 255;
  for (let y = 0; y < image.height; y++) samples[y * stride + stride - 1] &= mask;
}
writeFileSync(samplePath, samples);
writeFileSync(metadataPath, JSON.stringify({
  pdfjs_version: version, source_sha256: digest(source), width: image.width, height: image.height,
  sample_bytes: samples.length, sample_sha256: digest(samples),
  pdfjs_module_sha256: digest(readFileSync(modulePath)),
  pdfjs_worker_sha256: digest(readFileSync(path.join(path.dirname(modulePath), 'pdf.worker.mjs'))),
  pdfium_wasm_sha256: digest(readFileSync(path.join(wasmDirectory, 'jbig2.wasm'))),
  helper_sha256: digest(readFileSync(new URL(import.meta.url))),
}, null, 2) + '\n');
await document.destroy();
