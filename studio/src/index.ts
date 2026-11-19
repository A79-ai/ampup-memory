// AmpUp Memory's pages, as one package: Home, the memory's Explorer, Graph, Search and Ask, its Review and Owners,
// the Sessions it answered, its Advice, Learned, Not answered and People, and the Schema, Skills, Branches and Tools
// that build it. A host renders
// `<MemoryStudio transport={…} org=… />` with a `Transport` that reaches the memory (`LocalTransport` for the
// server that serves the pages), adds its own sections in the `goals` and `handover` slots, and imports
// `ampup-memory-studio/style.css` once. The type is Atkinson Hyperlegible where no NeuMontreal or Inter is
// installed: the host loads it (`@fontsource/atkinson-hyperlegible-next` 400/600/700 and `-mono` 400, as local.tsx
// does), so a library stylesheet doesn't carry the fonts inlined.

export {
  MemoryStudio,
  type MemoryStudioProps,
  type StudioNavGroup,
  type StudioNavigation,
  type StudioNavItem,
} from "./memory/Memory";
export {
  ApiError,
  errorText,
  type Area,
  type AreaOwner,
  type AreaOwnerKind,
  type AreaOwnerStore,
  type Arm,
  type ArmStore,
  type Capabilities,
  type Capability,
  type Transport,
} from "./transport";
export { LocalTransport } from "./local-transport";
export {
  NAVIGATE,
  learnedLink,
  optionsOf,
  replaceView,
  setOptions,
  subscribe,
  useChoice,
  useFlag,
  useOption,
  useSetOption,
  viewOf,
} from "./memory/hashOptions";
export { CopyId, EmptyState, ErrorState, Icon, Logo, Skeleton } from "./memory/ui";
export { Markdown } from "./memory/markdown";
export {
  Receipt,
  type ReceiptFact,
  type ReceiptItem,
  type ReceiptProps,
} from "./memory/receipt";
export {
  AddToMemory,
  AddToMemoryButton,
  AddToMemoryDialog,
  RememberThis,
  type AddToMemoryProps,
  type LearnItem,
} from "./memory/addToMemory";
export * from "./helpers";
