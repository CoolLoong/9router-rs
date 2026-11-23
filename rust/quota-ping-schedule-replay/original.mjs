import { readFileSync } from "node:fs";
import {
  getDuePingSlot,
  normalizePingSchedule,
} from "../../src/shared/services/quotaPingSchedule.js";

const {schedule, times} = JSON.parse(readFileSync(0, "utf8"));
const normalized = normalizePingSchedule(schedule);
console.log(JSON.stringify(times.map(time =>
  getDuePingSlot(normalized, Date.parse(time))?.key ?? null
)));
