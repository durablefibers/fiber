import { asciiLower, trim } from "@fiber/proto"

const RANK = { reader: 1, writer: 2, admin: 3, owner: 4 } as const
export type ProjectRole = keyof typeof RANK

export const atLeast = (role: ProjectRole, min: ProjectRole): boolean => RANK[role] >= RANK[min]

/** Unknown strings are `null`, so an unrecognised role grants nothing. */
export function parseRole(s: string): ProjectRole | null {
  const r = asciiLower(trim(s))
  return Object.hasOwn(RANK, r) ? (r as ProjectRole) : null
}
