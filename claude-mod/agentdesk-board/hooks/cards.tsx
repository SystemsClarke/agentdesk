import type { ClientModule } from 'claude-code'

/**
 * The list's cards as one Client region, so the whole card is the click target.
 *
 * A Button is pressable only on its own words, whatever box surrounds it. A
 * Client gets every pointer event over its region instead: a press anywhere on
 * a card (its border, its padding, the empty end of a short title) is mapped to
 * that card by its row and posted to the hooks module, which opens the thread.
 *
 * Every card is the same height, CARD_ROWS, so a row number is a card number:
 * border, title, details, border, then one blank row before the next.
 */
export type Card = {
  id: number
  title: string
  /** The coloured word that leads the details line ("permission", "open"), or ''. */
  badge: string
  badgeColor: string
  details: string
  isOpen: boolean
}
export type CardsProps = { cards: Card[] }
type State = { hover: number; focus: number }

export const CARD_ROWS = 5

const Cards: ClientModule<CardsProps, State> = (props, surface) => {
  const { Box, Text } = surface.elements
  const cards = props.cards ?? []
  const state = surface.state ?? { hover: -1, focus: -1 }
  const at = (y: number) => (y >= 0 ? Math.floor(y / CARD_ROWS) : -1)
  const inCard = (y: number) => y >= 0 && y % CARD_ROWS < CARD_ROWS - 1 && at(y) < cards.length

  surface.onPointer(e => {
    if (e.type === 'leave') {
      if (state.hover !== -1) surface.setState({ ...state, hover: -1 })
      return
    }
    // A held button reports moves past the region's edges too: only a pointer
    // inside the region is on a card. (columns is 0 until the first layout.)
    const isInside = e.x >= 0 && (surface.columns === 0 || e.x < surface.columns)
    const i = inCard(e.y) && isInside ? at(e.y) : -1
    if (e.type === 'move' && i !== state.hover) surface.setState({ ...state, hover: i })
    if (e.type === 'up' && e.button === 'left' && i !== -1) surface.post({ open: cards[i]!.id })
  })

  surface.onKey(k => {
    if (cards.length === 0) return
    const from = state.focus === -1 ? -1 : state.focus
    if (k.key === 'down') surface.setState({ ...state, focus: Math.min(cards.length - 1, from + 1) })
    if (k.key === 'up') surface.setState({ ...state, focus: Math.max(0, from - 1) })
    if (k.key === 'return' && from !== -1) surface.post({ open: cards[from]!.id })
  })

  return (
    <Box flexDirection="column" width="100%">
      {cards.map((c, i) => {
        const isLit = i === state.hover || i === state.focus
        return (
          <Box
            key={`card-${c.id}`}
            flexDirection="column"
            width="100%"
            height={CARD_ROWS - 1}
            marginBottom={1}
            paddingX={1}
            borderStyle="round"
            borderColor={c.isOpen || isLit ? 'claude' : 'promptBorder'}
            borderDimColor={!(c.isOpen || isLit)}
          >
            <Text bold color={c.isOpen ? 'claude' : 'text'} wrap="truncate-end">
              {c.title}
            </Text>
            <Text wrap="truncate-end">
              {c.badge !== '' && (
                <Text color={c.badgeColor} bold>
                  {c.badge}
                  {'  ·  '}
                </Text>
              )}
              <Text color="inactive">{c.details}</Text>
            </Text>
          </Box>
        )
      })}
    </Box>
  )
}

export default Cards
