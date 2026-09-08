import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs))
}

/** 32 位（trace）或 16 位（span）小写 hex；大写也认，用的时候转小写。 */
export function isHexId(s: string, len: 16 | 32): boolean {
  return new RegExp(`^[0-9a-fA-F]{${len}}$`).test(s.trim())
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}

/** 拆成一行一行，第一行和其余（堆栈）分开。 */
export function splitFirstLine(s: string): [string, string] {
  const idx = s.indexOf('\n')
  return idx < 0 ? [s, ''] : [s.slice(0, idx), s.slice(idx + 1)]
}
