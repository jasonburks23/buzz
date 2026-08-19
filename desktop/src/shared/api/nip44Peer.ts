import { invokeTauri } from "@/shared/api/tauri";

/**
 * Decrypt a NIP-44 message a peer (seat) addressed TO the operator, using
 * ECDH(operator_seckey, seatPubkeyHex). The badge folds each seat's published
 * read-state into the operator's effective read map via this path.
 */
export async function nip44DecryptFromPeer(
  ciphertext: string,
  seatPubkeyHex: string,
): Promise<string> {
  return invokeTauri<string>("nip44_decrypt_from_peer", {
    ciphertext,
    seatPubkeyHex,
  });
}
