// @nostos-sync/react-native — public entrypoint.
//
// Re-exports the TS facade and the TurboModule spec type. The default
// NativeNostos module instance is intentionally NOT re-exported — apps drive
// the facade; direct native-module access is for advanced / debugging paths.

export { NostosClient, NostosPushError } from "./NostosClient";
export type {
  NostosClientConfig,
  Row,
  Subscription,
  WatchSubscription,
  WriteOp,
  PushPlatform,
} from "./NostosClient";
export type { Spec as NativeNostosSpec } from "./NativeNostos";
