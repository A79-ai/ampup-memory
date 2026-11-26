import { createContext, useContext } from "react";

/**
 * Whether the person reading may change the memory (`MemoryStudio` gives it from the transport's `write`): false for
 * a viewer of the org, who only reads. Outside `MemoryStudio` (a host rendering a part alone) it is true, and the
 * server decides.
 */
export const CanWrite = createContext(true);

/** Whether the controls that change the memory are offered (see `CanWrite`). */
export const useCanWrite = () => useContext(CanWrite);
