import {
  fetchCatalogEvents,
  safeCatalogAvatarUrl,
  sharedCatalogHeads,
  type CatalogShareLevel,
} from "@/features/agents/lib/catalogRelay";
import type {
  AgentPersona,
  CatalogSourceCoordinate,
  RelayEvent,
  RespondToMode,
} from "@/shared/api/types";
import { KIND_PERSONA } from "@/shared/constants/kinds";

export type CatalogPersonaShareLevel = CatalogShareLevel;

type CatalogAgentProjection = {
  displayName: string;
  avatarUrl: string | null;
  systemPrompt: string;
  runtime: string | null;
  model: string | null;
  provider: string | null;
  namePool: string[];
  respondTo: RespondToMode | null;
  parallelism: number | null;
};

export type PersonaCatalogPublication = {
  eventId: string;
  ownerPubkey: string;
  sourcePersonaId: string;
  createdAt: number;
  agent: CatalogAgentProjection;
};

export type CatalogPersona = AgentPersona & {
  catalogSource: CatalogSourceCoordinate & {
    /** The publication event this projection was built from. */
    eventId: string;
    /** Whether the current identity published it. */
    isOwn: boolean;
  };
};

type JsonObject = Record<string, unknown>;

const MAX_AGENT_DISPLAY_NAME_CHARACTERS = 128;
const MAX_AGENT_SYSTEM_PROMPT_BYTES = 64 * 1_024;
const EMOJI_VARIATION_SELECTOR = 0xfe0f;
const ZERO_WIDTH_JOINER = 0x200d;
const EXTENDED_PICTOGRAPHIC_RE = /^\p{Extended_Pictographic}$/u;

function isProhibitedAgentTextCharacter(
  characters: readonly string[],
  index: number,
  allowLayoutControls: boolean,
): boolean {
  const character = characters[index];
  if (character === undefined) return false;
  const codePoint = character.codePointAt(0);
  if (codePoint === undefined) return false;

  const isControl =
    codePoint <= 0x1f || (codePoint >= 0x7f && codePoint <= 0x9f);
  const isAllowedLayoutControl =
    allowLayoutControls && (codePoint === 0x09 || codePoint === 0x0a);
  if (isControl && !isAllowedLayoutControl) return true;
  if (isAllowedEmojiFormatCharacter(characters, index)) return false;

  return (
    codePoint === 0x00ad ||
    codePoint === 0x034f ||
    codePoint === 0x061c ||
    (codePoint >= 0x115f && codePoint <= 0x1160) ||
    (codePoint >= 0x17b4 && codePoint <= 0x17b5) ||
    (codePoint >= 0x180b && codePoint <= 0x180f) ||
    (codePoint >= 0x200b && codePoint <= 0x200f) ||
    (codePoint >= 0x202a && codePoint <= 0x202e) ||
    (codePoint >= 0x2060 && codePoint <= 0x206f) ||
    codePoint === 0x3164 ||
    (codePoint >= 0xfe00 && codePoint <= 0xfe0f) ||
    codePoint === 0xfeff ||
    codePoint === 0xffa0 ||
    (codePoint >= 0xfff0 && codePoint <= 0xfff8) ||
    (codePoint >= 0x1bca0 && codePoint <= 0x1bca3) ||
    (codePoint >= 0x1d173 && codePoint <= 0x1d17a) ||
    (codePoint >= 0xe0000 && codePoint <= 0xe0fff)
  );
}

function isAllowedEmojiFormatCharacter(
  characters: readonly string[],
  index: number,
): boolean {
  const codePoint = characters[index]?.codePointAt(0);
  if (codePoint === EMOJI_VARIATION_SELECTOR) {
    const previous = characters[index - 1];
    return previous !== undefined && isEmojiVariationBase(previous);
  }
  if (codePoint !== ZERO_WIDTH_JOINER) return false;

  const next = characters[index + 1];
  return (
    hasPrecedingEmojiBase(characters, index) &&
    next !== undefined &&
    EXTENDED_PICTOGRAPHIC_RE.test(next)
  );
}

function hasPrecedingEmojiBase(
  characters: readonly string[],
  index: number,
): boolean {
  for (let previous = index - 1; previous >= 0; previous -= 1) {
    const character = characters[previous];
    const codePoint = character?.codePointAt(0);
    if (
      codePoint === EMOJI_VARIATION_SELECTOR ||
      (codePoint !== undefined && codePoint >= 0x1f3fb && codePoint <= 0x1f3ff)
    ) {
      continue;
    }
    return character !== undefined && EXTENDED_PICTOGRAPHIC_RE.test(character);
  }
  return false;
}

function isEmojiVariationBase(character: string): boolean {
  return (
    /^[#*0-9]$/u.test(character) || EXTENDED_PICTOGRAPHIC_RE.test(character)
  );
}

function isSafeAgentDefinitionText(
  displayName: string,
  systemPrompt: string,
): boolean {
  const displayNameCharacters = [...displayName];
  const systemPromptCharacters = [...systemPrompt];
  return (
    displayName.trim().length > 0 &&
    displayNameCharacters.length <= MAX_AGENT_DISPLAY_NAME_CHARACTERS &&
    new TextEncoder().encode(systemPrompt).length <=
      MAX_AGENT_SYSTEM_PROMPT_BYTES &&
    !displayNameCharacters.some((_character, index) =>
      isProhibitedAgentTextCharacter(displayNameCharacters, index, false),
    ) &&
    !systemPromptCharacters.some((_character, index) =>
      isProhibitedAgentTextCharacter(systemPromptCharacters, index, true),
    )
  );
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function optionalString(value: unknown): string | null {
  return typeof value === "string" && value.trim().length > 0 ? value : null;
}

function parsePersonaContent(event: RelayEvent): CatalogAgentProjection | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(event.content);
  } catch {
    return null;
  }
  if (!isObject(parsed)) return null;

  const displayName = parsed.display_name;
  const systemPrompt =
    typeof parsed.system_prompt === "string" ? parsed.system_prompt : "";
  if (
    typeof displayName !== "string" ||
    !isSafeAgentDefinitionText(displayName, systemPrompt)
  ) {
    return null;
  }

  const avatarUrl = safeCatalogAvatarUrl(parsed.avatar_url);
  const namePool = Array.isArray(parsed.name_pool)
    ? parsed.name_pool.filter(
        (candidate): candidate is string => typeof candidate === "string",
      )
    : [];
  const respondTo =
    parsed.respond_to === "allowlist"
      ? "owner-only"
      : parsed.respond_to === "owner-only" || parsed.respond_to === "anyone"
        ? parsed.respond_to
        : null;
  const parallelism =
    typeof parsed.parallelism === "number" &&
    Number.isInteger(parsed.parallelism) &&
    parsed.parallelism >= 1 &&
    parsed.parallelism <= 32
      ? parsed.parallelism
      : null;

  return {
    displayName,
    avatarUrl,
    systemPrompt,
    runtime: optionalString(parsed.runtime),
    model: optionalString(parsed.model),
    provider: optionalString(parsed.provider),
    namePool,
    respondTo,
    parallelism,
  };
}

/**
 * Project the shared kind:30175 heads onto persona publications.
 *
 * A head whose content does not parse is dropped, not retried against an older
 * event: [`sharedCatalogHeads`] already claimed the coordinate, so falling
 * back would resurrect a superseded definition.
 */
export function catalogPublicationsFromEvents(
  events: readonly RelayEvent[],
): PersonaCatalogPublication[] {
  const publications: PersonaCatalogPublication[] = [];

  for (const head of sharedCatalogHeads(events, KIND_PERSONA)) {
    const agent = parsePersonaContent(head.event);
    if (!agent) continue;
    publications.push({
      eventId: head.event.id,
      ownerPubkey: head.ownerPubkey,
      sourcePersonaId: head.dTag,
      createdAt: head.event.created_at,
      agent,
    });
  }

  return publications;
}

/** Read every shared persona event, page by page. */
export async function fetchPersonaCatalogPublications(): Promise<
  PersonaCatalogPublication[]
> {
  return catalogPublicationsFromEvents(await fetchCatalogEvents(KIND_PERSONA));
}

function publicationToPersona(
  publication: PersonaCatalogPublication,
  localPersona: AgentPersona | undefined,
  isOwn: boolean,
): CatalogPersona {
  const timestamp = new Date(publication.createdAt * 1_000).toISOString();
  // The publication remains authoritative for catalog presentation. An added
  // local copy contributes only the linkage id and selected state; merging the
  // whole copy would leak local edits (notably its avatar) into the publisher's
  // catalog entry.
  const basePersona: AgentPersona = {
    id:
      localPersona?.id ??
      `catalog:${publication.ownerPubkey}:${publication.sourcePersonaId}`,
    displayName: publication.agent.displayName,
    avatarUrl: publication.agent.avatarUrl,
    systemPrompt: publication.agent.systemPrompt,
    runtime: publication.agent.runtime,
    model: publication.agent.model,
    provider: publication.agent.provider,
    namePool: publication.agent.namePool,
    isBuiltIn: false,
    isActive: localPersona?.isActive ?? false,
    shared: true,
    sourceTeam: null,
    envVars: {},
    respondTo: publication.agent.respondTo,
    respondToAllowlist: [],
    parallelism: publication.agent.parallelism,
    createdAt: timestamp,
    updatedAt: timestamp,
  };

  return {
    ...basePersona,
    // Catalog membership is relay-confirmed by the shared event itself. Do not
    // let a local pending toggle override this projection.
    shared: true,
    catalogSource: {
      eventId: publication.eventId,
      ownerPubkey: publication.ownerPubkey,
      isOwn,
      personaId: publication.sourcePersonaId,
    },
  };
}

export function catalogPersonasFromPublications(
  publications: readonly PersonaCatalogPublication[],
  localPersonas: readonly AgentPersona[],
  currentPubkey: string | null | undefined,
): CatalogPersona[] {
  const normalizedCurrentPubkey = currentPubkey?.toLowerCase() ?? null;
  const personas: CatalogPersona[] = [];

  for (const publication of publications) {
    const isOwn = publication.ownerPubkey === normalizedCurrentPubkey;
    personas.push(
      publicationToPersona(
        publication,
        findLocalPersonaForCatalogEntry(localPersonas, {
          ownerPubkey: publication.ownerPubkey,
          personaId: publication.sourcePersonaId,
          isOwn,
        }),
        isOwn,
      ),
    );
  }

  return personas.sort((left, right) =>
    left.displayName.localeCompare(right.displayName),
  );
}

/**
 * The local persona backing a catalog entry, if the user already has it.
 *
 * An own publication is found by id — its `d`-tag *is* the local persona id. A
 * copy of another owner's entry carries a fresh local id instead, so the only
 * link back is the `catalogSource` coordinate stored on the copy. Matching on
 * that coordinate is what stops the catalog from offering "Add" for an entry
 * the user already added, which would mint a second copy.
 */
export function findLocalPersonaForCatalogEntry(
  localPersonas: readonly AgentPersona[],
  source: CatalogSourceCoordinate & { isOwn: boolean },
): AgentPersona | undefined {
  if (source.isOwn) {
    return localPersonas.find((persona) => persona.id === source.personaId);
  }
  return localPersonas.find(
    (persona) =>
      persona.catalogSource?.ownerPubkey === source.ownerPubkey &&
      persona.catalogSource?.personaId === source.personaId,
  );
}

export function isCatalogPersona(
  persona: AgentPersona,
): persona is CatalogPersona {
  return "catalogSource" in persona && isObject(persona.catalogSource);
}
