// What importing a file gives the bundled app: its URL.
declare module "*.svg" {
  const url: string;
  export default url;
}
