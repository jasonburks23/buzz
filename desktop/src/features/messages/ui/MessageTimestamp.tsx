import {
  formatFullDateTime,
  formatTimeWithoutDayPeriod,
} from "@/features/messages/lib/dateFormatters";
import { cn } from "@/shared/lib/cn";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/shared/ui/tooltip";

export function MessageTimestamp({
  className,
  createdAt,
  hideDayPeriod = false,
  time,
}: {
  className?: string;
  createdAt: number;
  hideDayPeriod?: boolean;
  time: string;
}) {
  const displayTime = hideDayPeriod ? formatTimeWithoutDayPeriod(time) : time;

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <p
          className={cn(
            "shrink-0 cursor-default whitespace-nowrap text-xs font-normal leading-4 tabular-nums text-muted-foreground/55",
            className,
          )}
          data-testid="message-timestamp"
        >
          {displayTime}
        </p>
      </TooltipTrigger>
      <TooltipContent side="top">
        {formatFullDateTime(createdAt)}
      </TooltipContent>
    </Tooltip>
  );
}
