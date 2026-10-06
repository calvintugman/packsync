import { enableViaApi } from "./world";
import type { World } from "./world";

export const NAME = "Skyblock Pack";

/**
 * Two computers changed the same instance independently: the laptop saved one file and uploaded it, the
 * desktop has a different new file on disk. The desktop's "Sync now" turns that into a conflict.
 */
export async function makeConflict(world: World, name = NAME): Promise<string> {
  const id = await enableViaApi(world, name);
  await world.control({ cmd: "laptop_restore", instance: name });
  await world.control({ cmd: "edit", device: "laptop", instance: name, path: "saves/World 0/stats/laptop.json", content: "laptop" });
  await world.control({ cmd: "sync", device: "laptop", instance: name });
  await world.control({ cmd: "edit", device: "desktop", instance: name, path: "saves/World 0/stats/desktop.json", content: "desktop" });
  return id;
}
