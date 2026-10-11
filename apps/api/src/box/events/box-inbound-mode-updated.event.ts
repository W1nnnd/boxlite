/*
 * Copyright 2026 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0
 */

import { Box } from '../entities/box.entity'
import { NetworkMode } from '../enums/network-mode.enum'

export class BoxInboundModeUpdatedEvent {
  constructor(
    public readonly box: Box,
    public readonly oldMode: NetworkMode,
    public readonly newMode: NetworkMode,
  ) {}
}
