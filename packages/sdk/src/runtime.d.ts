import type {
  Command,
  ItemBatch,
  ItemInput,
  MapFeature,
  PluginNotification,
  ProducedItem,
  ReportedCheck,
  ReportedStatus,
  SendCommand,
  Severity,
  Unit,
} from "@uxduck/protocol";

export declare function value(
  vehicleId: string | null | undefined,
  key: string,
  label: string,
  unit: Unit,
  number: number,
): ProducedItem;
export declare function text(
  vehicleId: string | null | undefined,
  key: string,
  label: string,
  words: string,
): ProducedItem;
export declare function table(
  vehicleId: string | null | undefined,
  key: string,
  label: string,
  rows: string[][],
): ProducedItem;
export declare function notify(
  vehicleId: string | null | undefined,
  severity: Severity,
  title: string,
  body?: string,
): PluginNotification;
export declare function check(
  vehicleId: string,
  id: string,
  status: ReportedStatus,
  detail?: string,
): ReportedCheck;
export declare function commandLong(
  vehicleId: string,
  command: number,
  params: number[],
): SendCommand & { command: Extract<Command, { type: "long" }> };
export declare function point(id: string, latitudeDeg: number, longitudeDeg: number): MapFeature;
export declare function circle(
  id: string,
  latitudeDeg: number,
  longitudeDeg: number,
  radiusM: number,
): MapFeature;
export declare function latest(
  batch: ItemBatch,
  vehicleId: string,
  key: string,
): ItemInput | undefined;
